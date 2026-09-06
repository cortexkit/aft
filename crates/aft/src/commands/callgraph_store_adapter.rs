use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tree_sitter::{Node, Parser};

use crate::callgraph::{self, TraceToSymbolCandidate};
use crate::callgraph_store::{
    CallGraphRead, CallGraphStoreError, StoreCallSite, StoreNode, StoreUnresolvedCall,
};
use crate::context::AppContext;
use crate::edit::line_col_to_byte;
use crate::error::AftError;
use crate::inspect::job::is_test_file;
use crate::parser::{
    detect_language, extract_symbols_from_tree, grammar_for, FileParser, SharedSymbolCache,
};
use crate::protocol::Response;
use crate::symbols::Symbol;

pub type StoreAdapterResult<T> = Result<T, CallGraphStoreError>;

const TRACE_DATA_RESOLVER_PROVENANCE: &str = "treesitter+resolver";
const HUB_SUMMARY_THRESHOLD: usize = 20;
const HUB_SUMMARY_LIMIT: usize = 15;
// The agent only receives 15 representative paths once a trace becomes a hub. A
// 10k expansion budget leaves ample room for ordinary traces while preventing a
// layered call graph from unfolding millions of path prefixes synchronously.
const TRACE_TO_EXPANSION_BUDGET: usize = 10_000;
const TRACE_TO_RETAINED_PATH_LIMIT: usize = HUB_SUMMARY_LIMIT * 4;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreHubSummary {
    pub message: String,
    pub total: usize,
    pub hidden_tests: usize,
    pub shown: usize,
    pub threshold: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub counts_are_lower_bounds: bool,
}

#[derive(Debug, Clone, Default)]
struct EdgeMarker {
    approximate: Option<bool>,
    resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallersResult {
    pub symbol: String,
    pub file: String,
    pub callers: Vec<StoreCallerGroup>,
    pub total_callers: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub scanned_files: usize,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallerGroup {
    pub file: String,
    pub callers: Vec<StoreCallerEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallerEntry {
    pub symbol: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallTreeNode {
    pub name: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    pub children: Vec<StoreCallTreeNode>,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreImpactResult {
    pub symbol: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub parameters: Vec<String>,
    pub total_affected: usize,
    pub affected_files: usize,
    pub callers: Vec<StoreImpactCaller>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreImpactCaller {
    pub caller_symbol: String,
    pub caller_file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub is_entry_point: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_expression: Option<String>,
    pub parameters: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceHop {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub is_entry_point: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTracePath {
    pub hops: Vec<StoreTraceHop>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceToResult {
    pub target_symbol: String,
    pub target_file: String,
    pub paths: Vec<StoreTracePath>,
    pub total_paths: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub total_paths_is_lower_bound: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub entry_points_found: usize,
    pub max_depth_reached: bool,
    pub truncated_paths: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceToSymbolHop {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceToSymbolResult {
    pub path: Option<Vec<StoreTraceToSymbolHop>>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone)]
enum ForwardCall {
    Resolved(StoreCallSite),
    Unresolved(StoreUnresolvedCall),
}

#[derive(Clone)]
enum TraceForwardCall {
    Resolved(StoreCallSite),
    Unresolved(StoreUnresolvedCall),
}

impl TraceForwardCall {
    fn byte_start(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_start,
            Self::Unresolved(call) => call.byte_start,
        }
    }

    fn byte_end(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_end,
            Self::Unresolved(call) => call.byte_end,
        }
    }

    fn line(&self) -> u32 {
        match self {
            Self::Resolved(site) => site.line,
            Self::Unresolved(call) => call.line,
        }
    }

    fn matches_position(&self, byte_start: usize, byte_end: usize) -> bool {
        self.byte_start() == byte_start && self.byte_end() == byte_end
    }
}

impl ForwardCall {
    fn byte_start(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_start,
            Self::Unresolved(call) => call.byte_start,
        }
    }

    fn line(&self) -> u32 {
        match self {
            Self::Resolved(site) => site.line,
            Self::Unresolved(call) => call.line,
        }
    }

    fn call_site_key(&self) -> (String, u32, String) {
        match self {
            Self::Resolved(site) => (
                site.caller.file.clone(),
                site.line,
                format!("{}::{}", site.target_file, site.target_symbol),
            ),
            Self::Unresolved(call) => (call.caller.file.clone(), call.line, call.symbol.clone()),
        }
    }
}

#[derive(Clone)]
struct ResolvedStoreSymbol {
    representative: StoreNode,
    nodes: Vec<StoreNode>,
}

#[derive(Clone)]
struct TraceElem {
    node: StoreNode,
    edge: EdgeMarker,
}

fn edge_marker(site: &StoreCallSite) -> EdgeMarker {
    if let Some(resolved_by) = site.supplemental_resolution() {
        EdgeMarker {
            approximate: Some(site.approximate()),
            resolved_by: Some(resolved_by.to_string()),
        }
    } else {
        EdgeMarker::default()
    }
}

fn edge_approximate(site: &StoreCallSite) -> Option<bool> {
    site.supplemental_resolution().map(|_| site.approximate())
}

fn edge_resolved_by(site: &StoreCallSite) -> Option<String> {
    site.supplemental_resolution().map(ToString::to_string)
}

fn test_hidden_summary(
    kind: &str,
    total: usize,
    hidden_tests: usize,
    shown: usize,
) -> StoreHubSummary {
    StoreHubSummary {
        message: format!(
            "Next: {total} {kind} ({hidden_tests} in tests, hidden — pass includeTests) — narrow with scope"
        ),
        total,
        hidden_tests,
        shown,
        threshold: HUB_SUMMARY_THRESHOLD,
        limit: HUB_SUMMARY_LIMIT,
        counts_are_lower_bounds: false,
    }
}

fn included_summary(
    kind: &str,
    total: usize,
    hidden_tests: usize,
    shown: usize,
) -> StoreHubSummary {
    let test_note = if hidden_tests == 0 {
        String::new()
    } else {
        format!(" ({hidden_tests} in tests, included)")
    };
    StoreHubSummary {
        message: format!("Next: {total} {kind}{test_note} — showing {shown}; narrow with scope"),
        total,
        hidden_tests,
        shown,
        threshold: HUB_SUMMARY_THRESHOLD,
        limit: HUB_SUMMARY_LIMIT,
        counts_are_lower_bounds: false,
    }
}

fn lower_bound_trace_summary(
    total: usize,
    hidden_tests: usize,
    shown: usize,
    include_tests: bool,
) -> StoreHubSummary {
    let test_note = if include_tests {
        if hidden_tests == 0 {
            " (test-path count also incomplete)".to_string()
        } else {
            format!(" (at least {hidden_tests} in tests, included)")
        }
    } else if hidden_tests == 0 {
        " (additional test paths may be uncounted — pass includeTests)".to_string()
    } else {
        format!(" (at least {hidden_tests} in tests, hidden — pass includeTests)")
    };
    StoreHubSummary {
        message: format!(
            "Next: at least {total} paths{test_note} — showing {shown}; traversal capped; narrow with scope"
        ),
        total,
        hidden_tests,
        shown,
        threshold: HUB_SUMMARY_THRESHOLD,
        limit: HUB_SUMMARY_LIMIT,
        counts_are_lower_bounds: true,
    }
}

fn callsite_is_from_test(site: &StoreCallSite) -> bool {
    is_test_file(&site.caller.file)
}

fn trace_path_starts_in_test(path: &StoreTracePath) -> bool {
    path.hops.first().is_some_and(|hop| is_test_file(&hop.file))
}

fn dedup_sites_for_summary(sites: Vec<StoreCallSite>) -> Vec<StoreCallSite> {
    let mut seen = BTreeSet::new();
    sites
        .into_iter()
        .filter(|site| seen.insert((site.caller.symbol.clone(), site.target_symbol.clone())))
        .collect()
}

fn trace_path_shape(path: &StoreTracePath) -> Vec<(String, String)> {
    path.hops
        .iter()
        .map(|hop| (hop.file.clone(), hop.symbol.clone()))
        .collect()
}

fn dedup_paths_for_summary(paths: Vec<StoreTracePath>) -> Vec<StoreTracePath> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(trace_path_shape(path)))
        .collect()
}

fn trace_path_order(left: &StoreTracePath, right: &StoreTracePath) -> std::cmp::Ordering {
    let left_entry = left
        .hops
        .first()
        .map(|hop| hop.symbol.as_str())
        .unwrap_or("");
    let right_entry = right
        .hops
        .first()
        .map(|hop| hop.symbol.as_str())
        .unwrap_or("");
    left_entry
        .cmp(right_entry)
        .then(left.hops.len().cmp(&right.hops.len()))
}

fn store_trace_path(elems: &[TraceElem]) -> StoreTracePath {
    let hops = elems
        .iter()
        .rev()
        .enumerate()
        .map(|(index, elem)| StoreTraceHop {
            symbol: elem.node.symbol.clone(),
            file: elem.node.file.clone(),
            line: elem.node.line,
            signature: elem.node.signature.clone(),
            is_entry_point: index == 0 && elem.node.is_entry_point,
            approximate: elem.edge.approximate,
            resolved_by: elem.edge.resolved_by.clone(),
        })
        .collect();
    StoreTracePath { hops }
}

fn retain_trace_path(retained: &mut Vec<StoreTracePath>, path: StoreTracePath) {
    retained.push(path);
    if retained.len() <= TRACE_TO_RETAINED_PATH_LIMIT {
        return;
    }
    retained.sort_by(trace_path_order);
    *retained = dedup_paths_for_summary(std::mem::take(retained))
        .into_iter()
        .take(TRACE_TO_RETAINED_PATH_LIMIT)
        .collect();
}

fn filter_call_tree_tests(node: &mut StoreCallTreeNode) {
    node.children.retain(|child| !is_test_file(&child.file));
    for child in &mut node.children {
        filter_call_tree_tests(child);
    }
}

pub fn callers_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreCallersResult> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let effective_depth = depth.max(1);
    let mut visited = HashSet::new();
    let mut sites = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;
    let callers_by_symbol = prefetch_callers(
        store,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
    )?;

    collect_callers_recursive(
        store,
        &callers_by_symbol,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
        0,
        &mut visited,
        &mut sites,
        &mut depth_limited,
        &mut truncated,
    )?;

    let mut sites = dedup_call_sites(sites);
    sites.sort_by(|left, right| {
        left.caller
            .file
            .cmp(&right.caller.file)
            .then(left.line.cmp(&right.line))
            .then(left.caller.symbol.cmp(&right.caller.symbol))
    });
    let total_callers = sites.len();
    let hidden_tests = sites
        .iter()
        .filter(|site| callsite_is_from_test(site))
        .count();
    let summarize = total_callers > HUB_SUMMARY_THRESHOLD;
    let visible_sites = sites
        .into_iter()
        .filter(|site| include_tests || !callsite_is_from_test(site))
        .collect::<Vec<_>>();
    let visible_sites = if summarize {
        dedup_sites_for_summary(visible_sites)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        visible_sites
    };
    let hub_summary = if summarize {
        Some(if include_tests {
            included_summary("callers", total_callers, hidden_tests, visible_sites.len())
        } else {
            test_hidden_summary("callers", total_callers, hidden_tests, visible_sites.len())
        })
    } else {
        None
    };
    let mut groups: BTreeMap<String, Vec<StoreCallerEntry>> = BTreeMap::new();
    for site in visible_sites {
        groups
            .entry(site.caller.file.clone())
            .or_default()
            .push(StoreCallerEntry {
                symbol: site.caller.symbol.clone(),
                line: site.line,
                approximate: edge_approximate(&site),
                resolved_by: edge_resolved_by(&site),
            });
    }

    Ok(StoreCallersResult {
        symbol: target.representative.symbol,
        file: target.representative.file,
        callers: groups
            .into_iter()
            .map(|(file, callers)| StoreCallerGroup { file, callers })
            .collect(),
        total_callers,
        hub_summary,
        scanned_files: store.indexed_file_count()?,
        depth_limited,
        truncated,
    })
}

pub fn call_tree_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreCallTreeNode> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let mut visited = HashSet::new();
    let mut adjacency_cache = HashMap::new();
    let mut tree = call_tree_inner(
        store,
        &target,
        depth,
        0,
        &mut visited,
        &mut adjacency_cache,
        true,
    )?;
    if !include_tests {
        filter_call_tree_tests(&mut tree);
    }
    Ok(tree)
}

pub fn impact_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreImpactResult> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let effective_depth = depth.max(1);
    let mut visited = HashSet::new();
    let mut sites = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;
    let callers_by_symbol = prefetch_callers(
        store,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
    )?;

    collect_callers_recursive(
        store,
        &callers_by_symbol,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
        0,
        &mut visited,
        &mut sites,
        &mut depth_limited,
        &mut truncated,
    )?;

    let mut sites = dedup_call_sites(sites);
    sites.sort_by(|left, right| {
        left.caller
            .file
            .cmp(&right.caller.file)
            .then(left.line.cmp(&right.line))
            .then(left.caller.symbol.cmp(&right.caller.symbol))
    });
    let total_affected = sites.len();
    let hidden_tests = sites
        .iter()
        .filter(|site| callsite_is_from_test(site))
        .count();
    let summarize = total_affected > HUB_SUMMARY_THRESHOLD;
    let affected_files = sites
        .iter()
        .map(|site| site.caller.file.clone())
        .collect::<BTreeSet<_>>()
        .len();
    let visible_sites = sites
        .into_iter()
        .filter(|site| include_tests || !callsite_is_from_test(site))
        .collect::<Vec<_>>();
    let visible_sites = if summarize {
        dedup_sites_for_summary(visible_sites)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        visible_sites
    };
    let hub_summary = if summarize {
        Some(if include_tests {
            included_summary(
                "affected callers",
                total_affected,
                hidden_tests,
                visible_sites.len(),
            )
        } else {
            test_hidden_summary(
                "affected callers",
                total_affected,
                hidden_tests,
                visible_sites.len(),
            )
        })
    } else {
        None
    };
    let target_signature = target.representative.signature.clone();
    let target_parameters = target_signature
        .as_deref()
        .map(|signature| callgraph::extract_parameters(signature, target.representative.lang))
        .unwrap_or_default();

    let mut callers = Vec::new();
    for site in visible_sites {
        callers.push(StoreImpactCaller {
            caller_symbol: site.caller.symbol.clone(),
            caller_file: site.caller.file.clone(),
            line: site.line,
            signature: site.caller.signature.clone(),
            is_entry_point: site.caller.is_entry_point,
            call_expression: read_source_line(
                &store.project_root().join(&site.caller.file),
                site.line,
            ),
            parameters: site
                .caller
                .signature
                .as_deref()
                .map(|signature| callgraph::extract_parameters(signature, site.caller.lang))
                .unwrap_or_default(),
            approximate: edge_approximate(&site),
            resolved_by: edge_resolved_by(&site),
        });
    }
    callers.sort_by(|left, right| {
        left.caller_file
            .cmp(&right.caller_file)
            .then(left.line.cmp(&right.line))
    });

    Ok(StoreImpactResult {
        symbol: target.representative.symbol,
        file: target.representative.file,
        signature: target_signature,
        parameters: target_parameters,
        total_affected,
        affected_files,
        callers,
        hub_summary,
        depth_limited,
        truncated,
    })
}

pub fn trace_to_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    max_depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreTraceToResult> {
    trace_to_result_with_budget(
        store,
        file,
        symbol,
        max_depth,
        include_tests,
        TRACE_TO_EXPANSION_BUDGET,
    )
    .map(|(result, _)| result)
}

fn trace_to_result_with_budget(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    max_depth: usize,
    include_tests: bool,
    expansion_budget: usize,
) -> StoreAdapterResult<(StoreTraceToResult, usize)> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let effective_max = if max_depth == 0 { 10 } else { max_depth };

    let initial = vec![TraceElem {
        node: target.representative.clone(),
        edge: EdgeMarker::default(),
    }];
    let mut retained_paths = Vec::new();
    let mut total_paths = 0usize;
    let mut hidden_tests = 0usize;
    if target.representative.is_entry_point {
        total_paths = 1;
        let path = store_trace_path(&initial);
        if trace_path_starts_in_test(&path) {
            hidden_tests = 1;
        }
        if include_tests || hidden_tests == 0 {
            retain_trace_path(&mut retained_paths, path);
        }
    }

    let mut queue = vec![(initial, 0usize)];
    let mut max_depth_reached = false;
    let mut truncated_paths = 0usize;
    let mut expansions = 0usize;
    let mut budget_exhausted = false;
    let mut callers_by_symbol: HashMap<(String, String), Vec<StoreCallSite>> = HashMap::new();

    'traversal: while let Some((path, depth)) = queue.pop() {
        if expansions >= expansion_budget {
            budget_exhausted = true;
            break;
        }
        expansions += 1;
        if depth >= effective_max {
            max_depth_reached = true;
            continue;
        }
        let Some(current) = path.last() else {
            continue;
        };
        let caller_key = (current.node.file.clone(), current.node.symbol.clone());
        if let std::collections::hash_map::Entry::Vacant(entry) =
            callers_by_symbol.entry(caller_key.clone())
        {
            let callers =
                dedup_call_sites(store.direct_callers_of(Path::new(&caller_key.0), &caller_key.1)?);
            entry.insert(callers);
        }
        let callers = callers_by_symbol
            .get(&caller_key)
            .expect("trace caller cache populated above");
        if callers.is_empty() {
            if path.len() > 1 {
                truncated_paths += 1;
            }
            continue;
        }

        let mut has_new_path = false;
        for site in callers {
            if path.iter().any(|elem| {
                elem.node.file == site.caller.file && elem.node.symbol == site.caller.symbol
            }) {
                continue;
            }
            has_new_path = true;
            let mut next_path = path.clone();
            if let Some(current) = next_path.last_mut() {
                current.edge = edge_marker(&site);
            }
            next_path.push(TraceElem {
                node: site.caller.clone(),
                edge: EdgeMarker::default(),
            });
            if site.caller.is_entry_point {
                total_paths = total_paths.saturating_add(1);
                let completed = store_trace_path(&next_path);
                let from_test = trace_path_starts_in_test(&completed);
                if from_test {
                    hidden_tests = hidden_tests.saturating_add(1);
                }
                if include_tests || !from_test {
                    retain_trace_path(&mut retained_paths, completed);
                }
            }
            // Reserve at most one future expansion slot per queued path. This
            // bounds queue memory as well as the number of paths actually popped.
            if expansions.saturating_add(queue.len()) >= expansion_budget {
                budget_exhausted = true;
                break 'traversal;
            }
            queue.push((next_path, depth + 1));
        }
        if !has_new_path && path.len() > 1 {
            truncated_paths += 1;
        }
    }

    retained_paths.sort_by(trace_path_order);
    let summarize = budget_exhausted || total_paths > HUB_SUMMARY_THRESHOLD;
    let paths = if summarize {
        dedup_paths_for_summary(retained_paths)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        retained_paths
    };
    let hub_summary = if summarize {
        Some(if budget_exhausted {
            lower_bound_trace_summary(total_paths, hidden_tests, paths.len(), include_tests)
        } else if include_tests {
            included_summary("paths", total_paths, hidden_tests, paths.len())
        } else {
            test_hidden_summary("paths", total_paths, hidden_tests, paths.len())
        })
    } else {
        None
    };

    let entry_points_found = paths
        .iter()
        .filter_map(|path| path.hops.first())
        .filter(|hop| hop.is_entry_point)
        .map(|hop| (hop.file.clone(), hop.symbol.clone()))
        .collect::<HashSet<_>>()
        .len();

    Ok((
        StoreTraceToResult {
            target_symbol: target.representative.symbol,
            target_file: target.representative.file,
            total_paths,
            total_paths_is_lower_bound: budget_exhausted,
            hub_summary,
            paths,
            entry_points_found,
            max_depth_reached,
            truncated_paths,
        },
        expansions,
    ))
}

pub fn ensure_symbol_resolves(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
) -> StoreAdapterResult<()> {
    resolve_symbol_query(store, file, symbol).map(|_| ())
}

pub fn trace_to_symbol_candidates(
    store: &impl CallGraphRead,
    to_symbol: &str,
) -> StoreAdapterResult<Vec<TraceToSymbolCandidate>> {
    store.trace_to_symbol_candidates(to_symbol)
}

pub fn trace_to_symbol_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    to_symbol: &str,
    to_file: Option<&Path>,
    max_depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreTraceToSymbolResult> {
    let origin = resolve_symbol_query(store, file, symbol)?;
    let target_file = to_file.map(|path| relative_file(store, path));
    let effective_max = if max_depth == 0 {
        10
    } else {
        max_depth.min(16)
    };

    let start_hop = trace_to_symbol_hop(&origin.representative);
    if trace_to_symbol_matches_target(
        &origin.representative.file,
        &origin.representative.symbol,
        to_symbol,
        target_file.as_deref(),
    ) {
        return Ok(StoreTraceToSymbolResult {
            path: Some(vec![start_hop]),
            complete: true,
            reason: None,
        });
    }

    let mut queue = VecDeque::new();
    queue.push_back((
        origin.representative.file.clone(),
        origin.representative.symbol.clone(),
        vec![start_hop],
        0usize,
    ));
    let mut visited = HashSet::new();
    visited.insert((
        origin.representative.file.clone(),
        origin.representative.symbol.clone(),
    ));
    let mut max_depth_exhausted = false;

    while !queue.is_empty() {
        let frontier_len = queue.len();
        let frontier = queue
            .iter()
            .take(frontier_len)
            .map(|(file, symbol, _, _)| (file.clone(), symbol.clone()))
            .collect::<Vec<_>>();
        let mut calls_by_symbol = store.outgoing_calls_for_symbols(&frontier)?;

        // Process the fetched frontier in the original queue order. Later-discovered
        // edges must not overtake shorter paths found by breadth-first traversal.
        for _ in 0..frontier_len {
            let Some((current_file, current_symbol, path, depth)) = queue.pop_front() else {
                break;
            };
            let calls = calls_by_symbol
                .remove(&(current_file, current_symbol))
                .unwrap_or_default();
            let callees = forward_resolved_callees(calls);

            if depth >= effective_max {
                if callees
                    .iter()
                    .any(|(node, _)| !visited.contains(&(node.file.clone(), node.symbol.clone())))
                {
                    max_depth_exhausted = true;
                }
                continue;
            }

            for (callee, edge) in callees {
                if !include_tests && is_test_file(&callee.file) {
                    continue;
                }
                if !visited.insert((callee.file.clone(), callee.symbol.clone())) {
                    continue;
                }
                let mut next_path = path.clone();
                next_path.push(trace_to_symbol_hop_with_edge(&callee, edge));
                if trace_to_symbol_matches_target(
                    &callee.file,
                    &callee.symbol,
                    to_symbol,
                    target_file.as_deref(),
                ) {
                    return Ok(StoreTraceToSymbolResult {
                        path: Some(next_path),
                        complete: true,
                        reason: None,
                    });
                }
                queue.push_back((callee.file, callee.symbol, next_path, depth + 1));
            }
        }
    }

    if max_depth_exhausted {
        Ok(StoreTraceToSymbolResult {
            path: None,
            complete: false,
            reason: Some("max_depth_exhausted".to_string()),
        })
    } else {
        Ok(StoreTraceToSymbolResult {
            path: None,
            complete: true,
            reason: Some("no_path_found".to_string()),
        })
    }
}

#[derive(Debug, Default)]
struct TrackedBindings {
    approximate_by_name: HashMap<String, bool>,
}

struct AssignmentInfo {
    binding: String,
    hop_variable: String,
    line: u32,
    approximate: bool,
    stop_after_hop: bool,
}

impl TrackedBindings {
    fn with_origin(name: &str, approximate: bool) -> Self {
        let mut bindings = Self::default();
        bindings.track(name.to_string(), approximate);
        bindings
    }

    fn contains(&self, name: &str) -> bool {
        self.approximate_by_name.contains_key(name)
    }

    fn approximation(&self, name: &str) -> Option<bool> {
        self.approximate_by_name.get(name).copied()
    }

    fn track(&mut self, name: String, approximate: bool) {
        self.approximate_by_name.insert(name, approximate);
    }

    fn kill(&mut self, name: &str) {
        self.approximate_by_name.remove(name);
    }

    fn mark_approximate(&mut self, name: &str) {
        if let Some(approximate) = self.approximate_by_name.get_mut(name) {
            *approximate = true;
        }
    }
}

pub fn trace_data_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    expression: &str,
    max_depth: usize,
    symbol_cache: SharedSymbolCache,
) -> StoreAdapterResult<callgraph::TraceDataResult> {
    let origin_path = absolute_file(store, file);
    let origin_file = relative_file(store, &origin_path);
    let origin_symbol = resolve_symbol_query_with_cache(&origin_path, symbol, &symbol_cache)?;

    let mut hops = Vec::new();
    let mut depth_limited = false;
    let mut visited = HashSet::new();
    trace_data_inner(
        store,
        &symbol_cache,
        &origin_path,
        &origin_symbol,
        expression,
        false,
        max_depth,
        0,
        &mut hops,
        &mut depth_limited,
        &mut visited,
    )?;

    Ok(callgraph::TraceDataResult {
        expression: expression.to_string(),
        origin_file,
        origin_symbol,
        hops,
        depth_limited,
    })
}

#[allow(clippy::too_many_arguments)]
fn trace_data_inner(
    store: &impl CallGraphRead,
    symbol_cache: &SharedSymbolCache,
    file: &Path,
    symbol: &str,
    tracking_name: &str,
    tracking_approximate: bool,
    max_depth: usize,
    current_depth: usize,
    hops: &mut Vec<callgraph::DataFlowHop>,
    depth_limited: &mut bool,
    visited: &mut HashSet<(String, String, String, bool)>,
) -> StoreAdapterResult<()> {
    let rel_file = relative_file(store, file);
    let visit_key = (
        rel_file.clone(),
        symbol.to_string(),
        tracking_name.to_string(),
        tracking_approximate,
    );
    if visited.contains(&visit_key) {
        return Ok(());
    }
    visited.insert(visit_key);

    let current = resolve_exact_symbol(store, &rel_file, symbol, None)?
        .ok_or_else(|| CallGraphStoreError::StaleFiles(vec![rel_file.clone()]))?;
    let current_calls = trace_forward_calls_for_nodes(store, &current.nodes)?;

    // Keep the legacy value-flow posture: parse the current source for body walks
    // and use the store only for cross-hop call facts.
    let source = std::fs::read_to_string(file)?;
    let Some(lang) = detect_language(file) else {
        return Ok(());
    };
    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|error| AftError::ParseError {
            message: format!("grammar init failed for {:?}: {}", lang, error),
        })?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| AftError::ParseError {
            message: format!("parse failed for {}", file.display()),
        })?;
    let symbols = extract_symbols_from_tree(&source, &tree, lang)?;
    let sym_info = symbols
        .iter()
        .find(|candidate| {
            symbol_identity_from_cache(candidate) == symbol || candidate.name == symbol
        })
        .ok_or_else(|| CallGraphStoreError::StaleFiles(vec![rel_file.clone()]))?;

    let body_start = line_col_to_byte(&source, sym_info.range.start_line, sym_info.range.start_col);
    let body_end = line_col_to_byte(&source, sym_info.range.end_line, sym_info.range.end_col);
    let Some(body_node) = find_node_covering_range(tree.root_node(), body_start, body_end) else {
        return Ok(());
    };

    let mut tracked = TrackedBindings::with_origin(tracking_name, tracking_approximate);
    walk_for_data_flow(
        store,
        symbol_cache,
        body_node,
        &source,
        &current_calls,
        &mut tracked,
        symbol,
        &rel_file,
        max_depth,
        current_depth,
        false,
        false,
        hops,
        depth_limited,
        visited,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_for_data_flow(
    store: &impl CallGraphRead,
    symbol_cache: &SharedSymbolCache,
    node: Node<'_>,
    source: &str,
    current_calls: &[TraceForwardCall],
    tracked: &mut TrackedBindings,
    symbol: &str,
    rel_file: &str,
    max_depth: usize,
    current_depth: usize,
    control_flow_uncertain: bool,
    origin_function_seen: bool,
    hops: &mut Vec<callgraph::DataFlowHop>,
    depth_limited: &mut bool,
    visited: &mut HashSet<(String, String, String, bool)>,
) -> StoreAdapterResult<()> {
    let kind = node.kind();
    let is_var_decl = matches!(
        kind,
        "variable_declarator"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "assignment"
            | "let_declaration"
            | "short_var_declaration"
    );

    if is_var_decl {
        if let Some(assignment) = extract_assignment_info(node, source, tracked) {
            let approximate = assignment.approximate || control_flow_uncertain;
            hops.push(callgraph::DataFlowHop {
                file: rel_file.to_string(),
                symbol: symbol.to_string(),
                variable: assignment.hop_variable,
                line: assignment.line,
                flow_type: "assignment".to_string(),
                approximate,
            });
            tracked.track(assignment.binding, approximate);
            if assignment.stop_after_hop {
                return Ok(());
            }
        } else if let Some(overwritten_name) = plain_assignment_target(node, source) {
            if tracked.contains(&overwritten_name) {
                if control_flow_uncertain {
                    // Because an assignment inside a conditional or loop may be skipped,
                    // retain the previously tracked path and mark subsequent uses as approximate.
                    tracked.mark_approximate(&overwritten_name);
                } else {
                    tracked.kill(&overwritten_name);
                }
            }
        }
    }

    if kind == "call_expression" || kind == "call" || kind == "macro_invocation" {
        check_call_for_data_flow(
            store,
            symbol_cache,
            node,
            source,
            current_calls,
            tracked,
            symbol,
            rel_file,
            max_depth,
            current_depth,
            hops,
            depth_limited,
            visited,
        )?;
    }

    let is_function = function_container(kind);
    let descendants_uncertain = control_flow_uncertain
        || conditional_flow_container(kind)
        || (is_function && origin_function_seen);
    let descendants_origin_function_seen = origin_function_seen || is_function;
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_for_data_flow(
                store,
                symbol_cache,
                cursor.node(),
                source,
                current_calls,
                tracked,
                symbol,
                rel_file,
                max_depth,
                current_depth,
                descendants_uncertain,
                descendants_origin_function_seen,
                hops,
                depth_limited,
                visited,
            )?;
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    Ok(())
}

fn extract_assignment_info(
    node: Node<'_>,
    source: &str,
    tracked: &TrackedBindings,
) -> Option<AssignmentInfo> {
    let kind = node.kind();
    let line = node.start_position().row as u32 + 1;

    let (name_node, value_node) = match kind {
        "variable_declarator" => (
            node.child_by_field_name("name")?,
            node.child_by_field_name("value")?,
        ),
        "assignment_expression" | "augmented_assignment_expression" | "assignment" => (
            node.child_by_field_name("left")?,
            node.child_by_field_name("right")?,
        ),
        "let_declaration" | "short_var_declaration" => (
            node.child_by_field_name("pattern")
                .or_else(|| node.child_by_field_name("left"))?,
            node.child_by_field_name("value")
                .or_else(|| node.child_by_field_name("right"))?,
        ),
        _ => return None,
    };

    let binding = trace_node_text(name_node, source);
    if name_node.kind() == "object_pattern" || name_node.kind() == "array_pattern" {
        tracked_reference_approximation(value_node, source, tracked)?;
        return Some(AssignmentInfo {
            binding: binding.clone(),
            hop_variable: binding,
            line,
            approximate: true,
            stop_after_hop: true,
        });
    }

    let source_approximate = if kind == "augmented_assignment_expression" {
        merge_reference_approximation(
            tracked_reference_approximation(name_node, source, tracked),
            tracked_reference_approximation(value_node, source, tracked),
        )?
    } else {
        tracked_reference_approximation(value_node, source, tracked)?
    };

    Some(AssignmentInfo {
        binding: binding.clone(),
        hop_variable: binding,
        line,
        approximate: source_approximate,
        stop_after_hop: false,
    })
}

fn merge_reference_approximation(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left && right),
        (Some(approximate), None) | (None, Some(approximate)) => Some(approximate),
        (None, None) => None,
    }
}

fn tracked_reference_approximation(
    node: Node<'_>,
    source: &str,
    tracked: &TrackedBindings,
) -> Option<bool> {
    let mut approximation = if is_identifier_reference(node) {
        tracked.approximation(&trace_node_text(node, source))
    } else {
        None
    };

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            approximation = merge_reference_approximation(
                approximation,
                tracked_reference_approximation(cursor.node(), source, tracked),
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    approximation
}

fn is_identifier_reference(node: Node<'_>) -> bool {
    if !matches!(
        node.kind(),
        "identifier" | "simple_identifier" | "variable_name" | "shorthand_property_identifier"
    ) {
        return false;
    }

    let Some(parent) = node.parent() else {
        return true;
    };
    !["property", "attribute", "field"].iter().any(|field| {
        parent
            .child_by_field_name(field)
            .is_some_and(|child| child.id() == node.id())
    })
}

fn plain_assignment_target(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "augmented_assignment_expression" {
        return None;
    }
    let target = match node.kind() {
        "assignment_expression" | "assignment" => node.child_by_field_name("left")?,
        _ => return None,
    };
    is_identifier_reference(target).then(|| trace_node_text(target, source))
}

fn conditional_flow_container(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement"
            | "else_clause"
            | "conditional_expression"
            | "ternary_expression"
            | "switch_statement"
            | "switch_expression"
            | "match_expression"
            | "when_expression"
            | "for_statement"
            | "for_in_statement"
            | "for_each_statement"
            | "while_statement"
            | "do_statement"
            | "try_statement"
            | "catch_clause"
            | "finally_clause"
    )
}

fn function_container(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "function_expression"
            | "arrow_function"
            | "method_definition"
            | "lambda"
            | "lambda_expression"
            | "closure_expression"
    )
}

#[allow(clippy::too_many_arguments)]
fn check_call_for_data_flow(
    store: &impl CallGraphRead,
    symbol_cache: &SharedSymbolCache,
    node: Node<'_>,
    source: &str,
    current_calls: &[TraceForwardCall],
    tracked: &TrackedBindings,
    symbol: &str,
    rel_file: &str,
    max_depth: usize,
    current_depth: usize,
    hops: &mut Vec<callgraph::DataFlowHop>,
    depth_limited: &mut bool,
    visited: &mut HashSet<(String, String, String, bool)>,
) -> StoreAdapterResult<()> {
    let args_node =
        find_child_by_kind(node, "arguments").or_else(|| find_child_by_kind(node, "argument_list"));
    let Some(args_node) = args_node else {
        return Ok(());
    };

    let mut arg_positions = Vec::new();
    let mut arg_idx = 0usize;
    let mut cursor = args_node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let child_kind = child.kind();
            if child_kind == "(" || child_kind == ")" || child_kind == "," {
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }

            let arg_text = trace_node_text(child, source);
            if child_kind == "spread_element" || child_kind == "dictionary_splat" {
                if tracked_reference_approximation(child, source, tracked).is_some() {
                    hops.push(callgraph::DataFlowHop {
                        file: rel_file.to_string(),
                        symbol: symbol.to_string(),
                        variable: arg_text,
                        line: child.start_position().row as u32 + 1,
                        flow_type: "parameter".to_string(),
                        approximate: true,
                    });
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
                arg_idx += 1;
                continue;
            }

            if let Some(approximate) = tracked.approximation(&arg_text) {
                arg_positions.push((arg_idx, arg_text, approximate));
            }

            arg_idx += 1;
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if arg_positions.is_empty() {
        return Ok(());
    }

    let matched_call = current_calls
        .iter()
        .find(|call| call.matches_position(node.start_byte(), node.end_byte()));

    match matched_call {
        Some(TraceForwardCall::Resolved(site)) => {
            let Some(target) = trace_target_node(store, site)? else {
                return Ok(());
            };
            if target.file != rel_file && current_depth + 1 > max_depth {
                *depth_limited = true;
                return Ok(());
            }
            let params = target
                .signature
                .as_deref()
                .map(|signature| callgraph::extract_parameters(signature, target.lang))
                .unwrap_or_default();
            let target_file = store.project_root().join(&target.file);
            for (pos, _tracked, approximate) in &arg_positions {
                if let Some(param_name) = params.get(*pos) {
                    hops.push(callgraph::DataFlowHop {
                        file: target.file.clone(),
                        symbol: target.symbol.clone(),
                        variable: param_name.clone(),
                        line: target.line,
                        flow_type: "parameter".to_string(),
                        approximate: *approximate,
                    });
                    trace_data_inner(
                        store,
                        symbol_cache,
                        &target_file,
                        &target.symbol,
                        param_name,
                        *approximate,
                        max_depth,
                        current_depth + 1,
                        hops,
                        depth_limited,
                        visited,
                    )?;
                }
            }
        }
        Some(TraceForwardCall::Unresolved(call)) => {
            push_unresolved_parameter_hops(hops, rel_file, &call.symbol, &arg_positions, node);
        }
        None => {
            let (_full_callee, short_callee) = extract_callee_names(node, source);
            if let Some(callee_name) = short_callee {
                push_unresolved_parameter_hops(hops, rel_file, &callee_name, &arg_positions, node);
            }
        }
    }

    Ok(())
}

fn push_unresolved_parameter_hops(
    hops: &mut Vec<callgraph::DataFlowHop>,
    rel_file: &str,
    callee_name: &str,
    arg_positions: &[(usize, String, bool)],
    call_node: Node<'_>,
) {
    for (_pos, tracked, _approximate) in arg_positions {
        hops.push(callgraph::DataFlowHop {
            file: rel_file.to_string(),
            symbol: callee_name.to_string(),
            variable: tracked.clone(),
            line: call_node.start_position().row as u32 + 1,
            flow_type: "parameter".to_string(),
            approximate: true,
        });
    }
}

fn trace_target_node(
    store: &impl CallGraphRead,
    site: &StoreCallSite,
) -> StoreAdapterResult<Option<StoreNode>> {
    if let Some(target) = &site.target {
        return Ok(Some(target.clone()));
    }
    resolve_exact_symbol(store, &site.target_file, &site.target_symbol, None)
        .map(|resolved| resolved.map(|symbol| symbol.representative))
}

fn trace_forward_calls_for_nodes(
    store: &impl CallGraphRead,
    nodes: &[StoreNode],
) -> StoreAdapterResult<Vec<TraceForwardCall>> {
    let mut calls = Vec::new();
    for node in nodes {
        calls.extend(
            store
                .outgoing_calls_of(node)?
                .into_iter()
                .filter(|site| site.resolved_by() == TRACE_DATA_RESOLVER_PROVENANCE)
                .map(TraceForwardCall::Resolved),
        );
        calls.extend(
            store
                .resolved_self_calls_of(node)?
                .into_iter()
                .filter(|site| site.resolved_by() == TRACE_DATA_RESOLVER_PROVENANCE)
                .map(TraceForwardCall::Resolved),
        );
        calls.extend(
            store
                .unresolved_calls_of(node)?
                .into_iter()
                .map(TraceForwardCall::Unresolved),
        );
    }
    calls.sort_by(|left, right| {
        left.byte_start()
            .cmp(&right.byte_start())
            .then(left.byte_end().cmp(&right.byte_end()))
            .then(left.line().cmp(&right.line()))
    });
    Ok(calls)
}

fn resolve_symbol_query_with_cache(
    file: &Path,
    symbol: &str,
    symbol_cache: &SharedSymbolCache,
) -> StoreAdapterResult<String> {
    let mut parser = FileParser::with_symbol_cache(symbol_cache.clone());
    let symbols = parser.extract_symbols(file)?;
    let candidates = symbol_query_candidates_from_symbols(&symbols, symbol);
    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => Err(AftError::SymbolNotFound {
            name: symbol.to_string(),
            file: file.display().to_string(),
        }
        .into()),
        _ => Err(AftError::AmbiguousSymbol {
            name: symbol.to_string(),
            candidates,
        }
        .into()),
    }
}

fn symbol_query_candidates_from_symbols(symbols: &[Symbol], symbol_name: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let qualified_query = symbol_name.contains("::");

    let mut consider = |candidate: String| {
        let matches = if qualified_query {
            candidate == symbol_name
        } else {
            candidate == symbol_name || unqualified_name(&candidate) == symbol_name
        };
        if matches && seen.insert(candidate.clone()) {
            candidates.push(candidate);
        }
    };

    for symbol in symbols {
        consider(symbol_identity_from_cache(symbol));
        if symbol.exported {
            consider(symbol.name.clone());
        }
    }

    candidates.sort();
    candidates
}

fn symbol_identity_from_cache(symbol: &Symbol) -> String {
    if symbol.scope_chain.is_empty() {
        symbol.name.clone()
    } else {
        format!("{}::{}", symbol.scope_chain.join("::"), symbol.name)
    }
}

fn trace_node_text(node: Node<'_>, source: &str) -> String {
    source[node.start_byte()..node.end_byte()].to_string()
}

fn find_node_covering_range(root: Node<'_>, start: usize, end: usize) -> Option<Node<'_>> {
    let mut best = None;
    let mut cursor = root.walk();

    fn walk_covering<'a>(
        cursor: &mut tree_sitter::TreeCursor<'a>,
        start: usize,
        end: usize,
        best: &mut Option<Node<'a>>,
    ) {
        let node = cursor.node();
        if node.start_byte() <= start && node.end_byte() >= end {
            *best = Some(node);
            if cursor.goto_first_child() {
                loop {
                    walk_covering(cursor, start, end, best);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
                cursor.goto_parent();
            }
        }
    }

    walk_covering(&mut cursor, start, end, &mut best);
    best
}

fn find_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == kind {
                return Some(cursor.node());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn extract_callee_names(node: Node<'_>, source: &str) -> (Option<String>, Option<String>) {
    let Some(callee) = node.child_by_field_name("function") else {
        return (None, None);
    };
    let full = trace_node_text(callee, source);
    let short = if full.contains('.') {
        full.rsplit('.').next().unwrap_or(&full).to_string()
    } else {
        full.clone()
    };
    (Some(full), Some(short))
}

pub fn serialized_value<T: Serialize>(
    req_id: &str,
    operation: &str,
    value: &T,
) -> Result<serde_json::Value, Response> {
    serde_json::to_value(value)
        .map_err(|error| serialization_error_response(req_id, operation, error))
}

pub fn serialized_response<T: Serialize>(req_id: &str, operation: &str, value: &T) -> Response {
    match serialized_value(req_id, operation, value) {
        Ok(value) => Response::success(req_id, value),
        Err(response) => response,
    }
}

pub fn serialization_error_response(
    req_id: &str,
    operation: &str,
    error: serde_json::Error,
) -> Response {
    Response::error(
        req_id,
        "serialization_error",
        format!("{operation}: failed to serialize response: {error}"),
    )
}

#[cfg(test)]
mod outbound_serialization_failure_tests {
    use super::*;

    struct AlwaysFails;

    impl serde::Serialize for AlwaysFails {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("forced serialization failure"))
        }
    }

    #[test]
    fn serialization_failure_returns_an_error_instead_of_a_successful_null() {
        let response = serialized_response("request", "callers", &AlwaysFails);
        assert!(!response.success);
        assert_eq!(response.data["code"], "serialization_error");
        assert!(response.data["message"]
            .as_str()
            .expect("message")
            .contains("forced serialization failure"));
    }
}

pub fn store_error_response(req_id: &str, operation: &str, error: CallGraphStoreError) -> Response {
    match error {
        CallGraphStoreError::Aft(error) => Response::error(req_id, error.code(), error.to_string()),
        CallGraphStoreError::Unavailable(message) => Response::error(
            req_id,
            "callgraph_unavailable",
            format!("{operation}: persisted callgraph store unavailable: {message}"),
        ),
        CallGraphStoreError::StaleFiles(files) => Response::error(
            req_id,
            "callgraph_stale",
            format!(
                "{operation}: persisted callgraph store has stale files: {}",
                files.join(", ")
            ),
        ),
        other => Response::error(
            req_id,
            "callgraph_store_error",
            format!("{operation}: persisted callgraph store error: {other}"),
        ),
    }
}

#[cfg(test)]
mod serialization_tests {
    use super::*;

    struct RefusesSerialization;

    impl Serialize for RefusesSerialization {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom(
                "deliberate serialization failure",
            ))
        }
    }

    #[test]
    fn serialization_failure_is_an_error_instead_of_default_success_data() {
        let response = serialized_response("request", "call_tree", &RefusesSerialization);
        assert!(!response.success);
        assert_eq!(response.data["code"], "serialization_error");
        assert!(response.data["message"]
            .as_str()
            .is_some_and(|message| message.contains("deliberate serialization failure")));
    }
}

pub fn note_callgraph_building(ctx: &crate::context::AppContext, tool: &str) {
    ctx.note_index_query(crate::logging::IndexPlane::Callgraph, tool, 0, "building");
}

pub fn note_callgraph_served(
    ctx: &crate::context::AppContext,
    tool: &str,
    service_ms: u64,
    status: &str,
) {
    ctx.note_index_query(
        crate::logging::IndexPlane::Callgraph,
        tool,
        service_ms,
        status,
    );
}

/// The persisted callgraph store is cold-building in the background. The op did
/// not block the request thread; the agent should retry shortly. Mirrors how
/// semantic search reports a build in progress.
pub fn building_response(req_id: &str, operation: &str) -> Response {
    Response::error(
        req_id,
        "callgraph_building",
        format!("{operation}: callgraph store is building in the background; retry shortly"),
    )
}

/// A tripped breaker is terminal for this request. It deliberately does not
/// reuse `Unavailable`: the store is configured, but its builder was suspended.
pub fn suspended_response(
    req_id: &str,
    operation: &str,
    suspension: &crate::build_breaker::BuildSuspension,
) -> Response {
    suspended_response_at(
        req_id,
        operation,
        suspension,
        crate::callgraph_store::unix_millis_now(),
    )
}

pub(crate) fn suspended_response_at(
    req_id: &str,
    operation: &str,
    suspension: &crate::build_breaker::BuildSuspension,
    now_ms: u64,
) -> Response {
    let age_ms = suspension.age_millis_at(now_ms);
    Response::error(
        req_id,
        "build_suspended",
        format!(
            "{operation}: build_suspended domain={} deaths={} age_ms={age_ms} reason={}; run doctor reset-build-breaker to resume",
            suspension.domain.as_str(),
            suspension.death_count,
            suspension.reason,
        ),
    )
}

/// Return the terminal navigation response for a HOME-root callgraph store.
///
/// This is intentionally distinct from `callgraph_unavailable`: HOME is
/// configured successfully, but it is not a project root and must never enter
/// the cold-build/retry loop.
pub fn home_root_disabled_response(req_id: &str, operation: &str) -> Response {
    Response::error_with_data(
        req_id,
        "callgraph_disabled",
        format!(
            "{operation}: callgraph store is disabled for home roots; open a project subdirectory to enable it"
        ),
        serde_json::json!({
            "status": "disabled",
            "reason": "home_root",
        }),
    )
}

pub fn unavailable_for(req_id: &str, operation: &str, ctx: &AppContext) -> Response {
    if ctx.is_home_root() {
        return home_root_disabled_response(req_id, operation);
    }
    unavailable_response(req_id, operation, ctx.is_worktree_bridge())
}

pub fn unavailable_response(req_id: &str, operation: &str, worktree: bool) -> Response {
    let message = if worktree {
        format!(
            "{operation}: persisted callgraph store is unavailable in this read-only worktree; run a callgraph operation in the main checkout to build it first"
        )
    } else {
        format!("{operation}: project not configured — send 'configure' first")
    };
    let code = if worktree {
        "callgraph_unavailable"
    } else {
        "not_configured"
    };
    Response::error(req_id, code, message)
}

fn resolve_symbol_query(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
) -> StoreAdapterResult<ResolvedStoreSymbol> {
    let nodes = store.nodes_for(file, symbol)?;
    collapse_symbol_nodes(store, file, symbol, nodes)
}

fn resolve_exact_symbol(
    store: &impl CallGraphRead,
    file: &str,
    symbol: &str,
    fallback: Option<StoreNode>,
) -> StoreAdapterResult<Option<ResolvedStoreSymbol>> {
    let nodes = store
        .nodes_for(Path::new(file), symbol)?
        .into_iter()
        .filter(|node| node.symbol == symbol)
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        return Ok(fallback.map(|node| ResolvedStoreSymbol {
            representative: node.clone(),
            nodes: vec![node],
        }));
    }
    Ok(Some(collapse_exact_nodes(nodes)))
}

fn collapse_symbol_nodes(
    store: &impl CallGraphRead,
    file: &Path,
    query: &str,
    nodes: Vec<StoreNode>,
) -> StoreAdapterResult<ResolvedStoreSymbol> {
    let mut by_symbol: BTreeMap<String, Vec<StoreNode>> = BTreeMap::new();
    for node in nodes {
        by_symbol.entry(node.symbol.clone()).or_default().push(node);
    }

    match by_symbol.len() {
        0 => Err(CallGraphStoreError::Aft(AftError::SymbolNotFound {
            name: query.to_string(),
            file: display_file_for_error(store, file),
        })),
        1 => Ok(collapse_exact_nodes(
            by_symbol.into_values().next().unwrap_or_default(),
        )),
        _ => Err(CallGraphStoreError::Aft(AftError::AmbiguousSymbol {
            name: query.to_string(),
            candidates: by_symbol.into_keys().collect(),
        })),
    }
}

fn collapse_exact_nodes(mut nodes: Vec<StoreNode>) -> ResolvedStoreSymbol {
    nodes.sort_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then(left.line.cmp(&right.line))
            .then(left.end_line.cmp(&right.end_line))
    });
    let representative = nodes[0].clone();
    ResolvedStoreSymbol {
        representative,
        nodes,
    }
}

fn prefetch_callers(
    store: &impl CallGraphRead,
    file: &str,
    symbol: &str,
    max_depth: usize,
) -> StoreAdapterResult<HashMap<(String, String), Vec<StoreCallSite>>> {
    let mut callers_by_symbol = HashMap::new();
    let mut frontier = BTreeSet::from([(file.to_string(), symbol.to_string())]);

    for depth in 0..max_depth {
        let targets = frontier
            .into_iter()
            .filter(|target| !callers_by_symbol.contains_key(target))
            .collect::<Vec<_>>();
        if targets.is_empty() {
            break;
        }
        let fetched = store.direct_callers_for_symbols(&targets)?;
        let mut next_frontier = BTreeSet::new();
        if depth + 1 < max_depth {
            for site in fetched.values().flatten() {
                next_frontier.insert((site.caller.file.clone(), site.caller.symbol.clone()));
            }
        }
        callers_by_symbol.extend(fetched);
        frontier = next_frontier;
    }

    Ok(callers_by_symbol)
}

#[allow(clippy::too_many_arguments)]
fn collect_callers_recursive(
    store: &impl CallGraphRead,
    callers_by_symbol: &HashMap<(String, String), Vec<StoreCallSite>>,
    file: &str,
    symbol: &str,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
    result: &mut Vec<StoreCallSite>,
    depth_limited: &mut bool,
    truncated: &mut usize,
) -> StoreAdapterResult<()> {
    if current_depth >= max_depth {
        let target = (file.to_string(), symbol.to_string());
        let counts = store.direct_caller_counts_of(std::slice::from_ref(&target))?;
        let omitted = counts.get(&target).copied().unwrap_or_default();
        if omitted > 0 {
            *depth_limited = true;
            *truncated += omitted;
        }
        return Ok(());
    }

    if !visited.insert((file.to_string(), symbol.to_string())) {
        return Ok(());
    }

    let Some(sites) = callers_by_symbol.get(&(file.to_string(), symbol.to_string())) else {
        return Ok(());
    };
    if sites.is_empty() {
        return Ok(());
    }
    if current_depth + 1 < max_depth {
        for site in sites {
            result.push(site.clone());
            collect_callers_recursive(
                store,
                callers_by_symbol,
                &site.caller.file,
                &site.caller.symbol,
                max_depth,
                current_depth + 1,
                visited,
                result,
                depth_limited,
                truncated,
            )?;
        }
    } else {
        let boundary_targets = sites
            .iter()
            .map(|site| (site.caller.file.clone(), site.caller.symbol.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let boundary_counts = store.direct_caller_counts_of(&boundary_targets)?;
        for site in sites {
            result.push(site.clone());
            let key = (site.caller.file.clone(), site.caller.symbol.clone());
            let omitted = boundary_counts.get(&key).copied().unwrap_or_default();
            if omitted > 0 {
                *depth_limited = true;
                *truncated += omitted;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn call_tree_inner(
    store: &impl CallGraphRead,
    current: &ResolvedStoreSymbol,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
    adjacency_cache: &mut HashMap<(String, String), Vec<ForwardCall>>,
    memoize_adjacency: bool,
) -> StoreAdapterResult<StoreCallTreeNode> {
    let node = &current.representative;
    let visit_key = (node.file.clone(), node.symbol.clone());
    if visited.contains(&visit_key) {
        return Ok(StoreCallTreeNode {
            name: node.symbol.clone(),
            file: node.file.clone(),
            line: node.line,
            signature: node.signature.clone(),
            resolved: true,
            approximate: None,
            resolved_by: None,
            children: Vec::new(),
            depth_limited: false,
            truncated: 0,
        });
    }
    visited.insert(visit_key.clone());

    // Only adjacency rows are shared across converging paths. The visited set remains
    // path-local so repeated nodes still render independently with correct cycle guards.
    let calls = if memoize_adjacency {
        if let Some(calls) = adjacency_cache.get(&visit_key) {
            calls.clone()
        } else {
            let calls = forward_calls_for_nodes(store, &current.nodes)?;
            adjacency_cache.insert(visit_key.clone(), calls.clone());
            calls
        }
    } else {
        forward_calls_for_nodes(store, &current.nodes)?
    };
    let mut children = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;

    if current_depth < max_depth {
        for call in calls {
            match call {
                ForwardCall::Resolved(site) => {
                    let resolved = resolve_exact_symbol(
                        store,
                        &site.target_file,
                        &site.target_symbol,
                        site.target.clone(),
                    )?;
                    if let Some(child_symbol) = resolved {
                        let mut child = call_tree_inner(
                            store,
                            &child_symbol,
                            max_depth,
                            current_depth + 1,
                            visited,
                            adjacency_cache,
                            memoize_adjacency,
                        )?;
                        child.approximate = edge_approximate(&site);
                        child.resolved_by = edge_resolved_by(&site);
                        depth_limited |= child.depth_limited;
                        truncated += child.truncated;
                        children.push(child);
                    } else {
                        children.push(StoreCallTreeNode {
                            name: site.target_symbol.clone(),
                            file: site.target_file.clone(),
                            line: site.line,
                            signature: None,
                            resolved: false,
                            approximate: edge_approximate(&site),
                            resolved_by: edge_resolved_by(&site),
                            children: Vec::new(),
                            depth_limited: false,
                            truncated: 0,
                        });
                    }
                }
                ForwardCall::Unresolved(call) => children.push(StoreCallTreeNode {
                    name: call.symbol,
                    file: call.caller.file,
                    line: call.line,
                    signature: None,
                    resolved: false,
                    approximate: None,
                    resolved_by: None,
                    children: Vec::new(),
                    depth_limited: false,
                    truncated: 0,
                }),
            }
        }
    } else if !calls.is_empty() {
        depth_limited = true;
        truncated = calls.len();
    }

    visited.remove(&visit_key);
    Ok(StoreCallTreeNode {
        name: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
        signature: node.signature.clone(),
        resolved: true,
        approximate: None,
        resolved_by: None,
        children,
        depth_limited,
        truncated,
    })
}

fn forward_calls_for_nodes(
    store: &impl CallGraphRead,
    nodes: &[StoreNode],
) -> StoreAdapterResult<Vec<ForwardCall>> {
    let mut calls = Vec::new();
    for node in nodes {
        calls.extend(
            store
                .outgoing_calls_of(node)?
                .into_iter()
                .map(ForwardCall::Resolved),
        );
        calls.extend(
            store
                .unresolved_calls_of(node)?
                .into_iter()
                .map(ForwardCall::Unresolved),
        );
    }
    calls.sort_by(|left, right| {
        left.byte_start()
            .cmp(&right.byte_start())
            .then(left.line().cmp(&right.line()))
    });
    let mut seen = BTreeSet::new();
    calls.retain(|call| seen.insert(call.call_site_key()));
    Ok(calls)
}

fn forward_resolved_callees(calls: Vec<StoreCallSite>) -> Vec<(StoreNode, EdgeMarker)> {
    let mut calls = dedup_call_sites(calls);
    calls.sort_by(|left, right| {
        left.byte_start
            .cmp(&right.byte_start)
            .then(left.line.cmp(&right.line))
    });

    calls
        .into_iter()
        .filter_map(|site| {
            let edge = edge_marker(&site);
            site.target.map(|target| (target, edge))
        })
        .collect()
}

fn dedup_call_sites(sites: Vec<StoreCallSite>) -> Vec<StoreCallSite> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for site in sites {
        if seen.insert(call_site_key(&site)) {
            deduped.push(site);
        }
    }
    deduped
}

#[cfg(test)]
fn dedup_call_site_count(sites: Vec<StoreCallSite>) -> usize {
    sites
        .into_iter()
        .map(|site| call_site_key(&site))
        .collect::<HashSet<_>>()
        .len()
}

fn call_site_key(site: &StoreCallSite) -> (String, u32, String, String) {
    (
        site.caller.file.clone(),
        site.line,
        site.target_file.clone(),
        site.target_symbol.clone(),
    )
}

fn trace_to_symbol_hop(node: &StoreNode) -> StoreTraceToSymbolHop {
    trace_to_symbol_hop_with_edge(node, EdgeMarker::default())
}

fn trace_to_symbol_hop_with_edge(node: &StoreNode, edge: EdgeMarker) -> StoreTraceToSymbolHop {
    StoreTraceToSymbolHop {
        symbol: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
        approximate: edge.approximate,
        resolved_by: edge.resolved_by,
    }
}

fn trace_to_symbol_matches_target(
    file: &str,
    symbol: &str,
    to_symbol: &str,
    to_file: Option<&str>,
) -> bool {
    if !(symbol == to_symbol || unqualified_name(symbol) == to_symbol) {
        return false;
    }
    match to_file {
        Some(target_file) => file == target_file,
        None => true,
    }
}

fn unqualified_name(symbol: &str) -> &str {
    symbol.rsplit("::").next().unwrap_or(symbol)
}

fn read_source_line(path: &Path, line: u32) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .map(|line| line.trim().to_string())
}

fn display_file_for_error(store: &impl CallGraphRead, file: &Path) -> String {
    absolute_file(store, file).display().to_string()
}

fn relative_file(store: &impl CallGraphRead, file: &Path) -> String {
    let absolute = absolute_file(store, file);
    absolute
        .strip_prefix(store.project_root())
        .unwrap_or(&absolute)
        .to_string_lossy()
        .replace('\\', "/")
}

fn absolute_file(store: &impl CallGraphRead, file: &Path) -> PathBuf {
    let full_path = if file.is_relative() {
        store.project_root().join(file)
    } else {
        file.to_path_buf()
    };
    std::fs::canonicalize(&full_path).unwrap_or(full_path)
}

#[cfg(test)]
mod trace_to_tests {
    use super::*;
    use crate::callgraph_store::{
        Result as CallGraphResult, StoreCallersResult as RawCallersResult,
        StoreImpactResult as RawImpactResult, StoredEdge,
    };
    use std::cell::RefCell;

    struct CountingStore {
        root: PathBuf,
        sqlite_path: PathBuf,
        nodes: HashMap<(String, String), StoreNode>,
        callers: HashMap<(String, String), Vec<StoreCallSite>>,
        outgoing: HashMap<(String, String), Vec<StoreCallSite>>,
        caller_queries: RefCell<HashMap<(String, String), usize>>,
        caller_frontier_query_count: RefCell<usize>,
        caller_frontier_target_count: RefCell<usize>,
        forward_query_count: RefCell<usize>,
        frontier_query_count: RefCell<usize>,
        caller_count_queries: RefCell<usize>,
        caller_count_targets: RefCell<usize>,
    }

    impl CountingStore {
        fn new() -> Self {
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
            }
        }

        fn add_node(&mut self, node: StoreNode) {
            self.nodes
                .insert((node.file.clone(), node.symbol.clone()), node);
        }

        fn add_caller(&mut self, target: &StoreNode, caller: &StoreNode) {
            self.add_caller_at(target, caller, caller.line);
        }

        fn add_caller_at(&mut self, target: &StoreNode, caller: &StoreNode, line: u32) {
            self.callers
                .entry((target.file.clone(), target.symbol.clone()))
                .or_default()
                .push(StoreCallSite {
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

        fn add_outgoing(&mut self, caller: &StoreNode, target: &StoreNode) {
            self.outgoing
                .entry((caller.file.clone(), caller.symbol.clone()))
                .or_default()
                .push(StoreCallSite {
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

        fn total_forward_queries(&self) -> usize {
            *self.forward_query_count.borrow()
        }

        fn reset_forward_queries(&self) {
            *self.forward_query_count.borrow_mut() = 0;
        }

        fn total_frontier_queries(&self) -> usize {
            *self.frontier_query_count.borrow()
        }

        fn total_caller_queries(&self) -> usize {
            self.caller_queries.borrow().values().sum()
        }

        fn total_caller_frontier_queries(&self) -> usize {
            *self.caller_frontier_query_count.borrow()
        }

        fn caller_frontier_target_count(&self) -> usize {
            *self.caller_frontier_target_count.borrow()
        }

        fn total_caller_count_queries(&self) -> usize {
            *self.caller_count_queries.borrow()
        }

        fn caller_count_target_count(&self) -> usize {
            *self.caller_count_targets.borrow()
        }

        fn reset_query_counts(&self) {
            self.caller_queries.borrow_mut().clear();
            *self.caller_frontier_query_count.borrow_mut() = 0;
            *self.caller_frontier_target_count.borrow_mut() = 0;
            *self.caller_count_queries.borrow_mut() = 0;
            *self.caller_count_targets.borrow_mut() = 0;
        }
    }

    impl CallGraphRead for CountingStore {
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
            let key = (
                file_rel.to_string_lossy().replace('\\', "/"),
                symbol.to_string(),
            );
            Ok(self.nodes.get(&key).cloned().into_iter().collect())
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
            unreachable!("not used by these adapter tests")
        }

        fn unresolved_calls_of(
            &self,
            _node: &StoreNode,
        ) -> CallGraphResult<Vec<StoreUnresolvedCall>> {
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

    fn node(symbol: &str, is_entry_point: bool) -> StoreNode {
        StoreNode::for_test(&format!("{symbol}.ts"), symbol, is_entry_point)
    }

    fn layered_store(width: usize, layers: usize) -> (CountingStore, StoreNode) {
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

    fn converging_call_tree_store(width: usize) -> (CountingStore, StoreNode) {
        let mut store = CountingStore::new();
        let root = node("root", false);
        let helper = node("helper", false);
        let leaf = node("leaf", false);
        for fixture_node in [&root, &helper, &leaf] {
            store.add_node(fixture_node.clone());
        }
        store.add_outgoing(&helper, &leaf);

        for index in 0..width {
            let handler = node(&format!("handler_{index}"), false);
            store.add_node(handler.clone());
            store.add_outgoing(&root, &handler);
            store.add_outgoing(&handler, &helper);
        }
        (store, root)
    }

    fn call_tree_node_count(tree: &StoreCallTreeNode) -> usize {
        1 + tree
            .children
            .iter()
            .map(call_tree_node_count)
            .sum::<usize>()
    }

    #[test]
    fn call_tree_memoizes_only_adjacency_and_preserves_rendered_tree() {
        let (store, root) = converging_call_tree_store(200);

        let memoized = call_tree_result(&store, Path::new(&root.file), &root.symbol, 3, true)
            .expect("memoized call tree");
        let memoized_queries = store.total_forward_queries();

        store.reset_forward_queries();
        let resolved_root = ResolvedStoreSymbol {
            representative: root.clone(),
            nodes: vec![root],
        };
        let mut visited = HashSet::new();
        let mut unused_cache = HashMap::new();
        let uncached = call_tree_inner(
            &store,
            &resolved_root,
            3,
            0,
            &mut visited,
            &mut unused_cache,
            false,
        )
        .expect("uncached call tree");
        let uncached_queries = store.total_forward_queries();

        assert_eq!(call_tree_node_count(&memoized), 601);
        assert_eq!(
            serde_json::to_vec(&memoized).expect("serialize memoized tree"),
            serde_json::to_vec(&uncached).expect("serialize uncached tree"),
            "adjacency memoization must not change rendered call-tree bytes"
        );
        assert_eq!(uncached_queries, 1_202);
        assert_eq!(memoized_queries, 406);
    }

    #[test]
    fn reverse_frontiers_batch_caller_fetches_by_depth() {
        let (store, target) = layered_store(20, 6);

        let result = callers_result(&store, Path::new(&target.file), &target.symbol, 5, true)
            .expect("callers result");

        assert_eq!(store.total_caller_queries(), 0);
        assert_eq!(store.total_caller_frontier_queries(), 5);
        assert_eq!(store.caller_frontier_target_count(), 81);
        assert_eq!(result.total_callers, 1_620);
        assert_eq!(result.callers.len(), 15);
        assert!(result.depth_limited);
    }

    #[test]
    fn batched_boundary_counts_preserve_serialized_callers_and_impact_contract() {
        let mut store = CountingStore::new();
        let target = node("target", false);
        let boundary = node("hubCaller", false);
        let upstream_a = node("upstreamA", true);
        let upstream_b = node("upstreamB", true);
        for fixture_node in [&target, &boundary, &upstream_a, &upstream_b] {
            store.add_node(fixture_node.clone());
        }
        for line in 1..=21 {
            store.add_caller_at(&target, &boundary, line);
        }
        store.add_caller(&boundary, &upstream_a);
        store.add_caller(&boundary, &upstream_b);

        let callers = callers_result(&store, Path::new(&target.file), &target.symbol, 1, true)
            .expect("callers result");
        assert_eq!(store.total_caller_queries(), 0);
        assert_eq!(store.total_caller_frontier_queries(), 1);
        assert_eq!(store.caller_frontier_target_count(), 1);
        assert_eq!(store.total_caller_count_queries(), 1);
        assert_eq!(store.caller_count_target_count(), 1);
        assert_eq!(
            serde_json::to_string(&callers).expect("serialize callers result"),
            r#"{"symbol":"target","file":"target.ts","callers":[{"file":"hubCaller.ts","callers":[{"symbol":"hubCaller","line":1}]}],"total_callers":21,"hub_summary":{"message":"Next: 21 callers — showing 1; narrow with scope","total":21,"hidden_tests":0,"shown":1,"threshold":20,"limit":15},"scanned_files":4,"depth_limited":true,"truncated":42}"#
        );

        store.reset_query_counts();
        let impact = impact_result(&store, Path::new(&target.file), &target.symbol, 1, true)
            .expect("impact result");
        assert_eq!(store.total_caller_queries(), 0);
        assert_eq!(store.total_caller_frontier_queries(), 1);
        assert_eq!(store.caller_frontier_target_count(), 1);
        assert_eq!(store.total_caller_count_queries(), 1);
        assert_eq!(store.caller_count_target_count(), 1);
        assert_eq!(
            serde_json::to_string(&impact).expect("serialize impact result"),
            r#"{"symbol":"target","file":"target.ts","parameters":[],"total_affected":21,"affected_files":1,"callers":[{"caller_symbol":"hubCaller","caller_file":"hubCaller.ts","line":1,"is_entry_point":false,"parameters":[]}],"hub_summary":{"message":"Next: 21 affected callers — showing 1; narrow with scope","total":21,"hidden_tests":0,"shown":1,"threshold":20,"limit":15},"depth_limited":true,"truncated":42}"#
        );
    }

    #[test]
    fn trace_to_symbol_batches_frontiers_without_changing_shortest_path() {
        let mut store = CountingStore::new();
        let origin = node("origin", false);
        let slow = node("slow", false);
        let slow_middle = node("slowMiddle", false);
        let fast = node("fast", false);
        let target = node("target", false);
        for fixture_node in [&origin, &slow, &slow_middle, &fast, &target] {
            store.add_node(fixture_node.clone());
        }
        store.add_outgoing(&origin, &slow);
        store.add_outgoing(&origin, &fast);
        store.add_outgoing(&slow, &slow_middle);
        store.add_outgoing(&slow_middle, &target);
        store.add_outgoing(&fast, &target);

        let result = trace_to_symbol_result(
            &store,
            Path::new(&origin.file),
            &origin.symbol,
            &target.symbol,
            None,
            10,
            true,
        )
        .expect("trace-to-symbol result");
        let symbols = result
            .path
            .expect("shortest path")
            .into_iter()
            .map(|hop| hop.symbol)
            .collect::<Vec<_>>();

        assert_eq!(symbols, vec!["origin", "fast", "target"]);
        assert_eq!(store.total_frontier_queries(), 2);
        assert_eq!(store.total_forward_queries(), 0);
    }

    #[test]
    fn trace_to_caches_callers_for_convergent_path_prefixes() {
        let (store, target) = layered_store(2, 3);

        let (result, expansions) = trace_to_result_with_budget(
            &store,
            Path::new(&target.file),
            &target.symbol,
            10,
            true,
            100,
        )
        .expect("trace result");

        assert_eq!(result.total_paths, 8);
        assert!(!result.total_paths_is_lower_bound);
        assert_eq!(expansions, 15);
        assert_eq!(store.total_caller_queries(), 7);
        assert!(store
            .caller_queries
            .borrow()
            .values()
            .all(|queries| *queries == 1));
    }

    #[test]
    fn trace_to_budget_returns_valid_paths_and_marks_counts_as_lower_bounds() {
        let (store, target) = layered_store(2, 3);

        let (result, expansions) = trace_to_result_with_budget(
            &store,
            Path::new(&target.file),
            &target.symbol,
            10,
            true,
            10,
        )
        .expect("trace result");

        assert!(result.total_paths_is_lower_bound);
        assert!(result.total_paths > 0);
        assert!(expansions <= 10);
        assert!(store.total_caller_queries() <= 10);
        let summary = result.hub_summary.expect("lower-bound summary");
        assert!(summary.counts_are_lower_bounds);
        assert!(summary.message.contains("at least"));
        for path in result.paths {
            assert!(path.hops.first().is_some_and(|hop| hop.is_entry_point));
            assert_eq!(
                path.hops.last().map(|hop| hop.symbol.as_str()),
                Some("target")
            );
        }
    }

    #[test]
    fn trace_to_below_budget_preserves_exact_serialized_contract() {
        let mut store = CountingStore::new();
        let target = node("target", false);
        let middle = node("middle", false);
        let entry = node("entry", true);
        for fixture_node in [&target, &middle, &entry] {
            store.add_node(fixture_node.clone());
        }
        store.add_caller(&target, &middle);
        store.add_caller(&middle, &entry);

        let (result, _expansions) = trace_to_result_with_budget(
            &store,
            Path::new(&target.file),
            &target.symbol,
            10,
            true,
            100,
        )
        .expect("trace result");

        assert_eq!(
            serde_json::to_string(&result).expect("serialize trace result"),
            r#"{"target_symbol":"target","target_file":"target.ts","paths":[{"hops":[{"symbol":"entry","file":"entry.ts","line":1,"is_entry_point":true},{"symbol":"middle","file":"middle.ts","line":1,"is_entry_point":false},{"symbol":"target","file":"target.ts","line":1,"is_entry_point":false}]}],"total_paths":1,"entry_points_found":1,"max_depth_reached":false,"truncated_paths":1}"#
        );
    }
}
