use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde::Serialize;
use tree_sitter::{Node, Parser};

use crate::callgraph::{self, TraceToSymbolCandidate};
use crate::callgraph_store::{
    CallGraphRead, CallGraphStoreError, StoreCallSite, StoreNode, StoreUnresolvedCall,
    MACRO_BODY_CALL_REF_KIND, MACRO_MENTION_REF_KIND, UNRESOLVED_CALL_SITE_COUNT_LIMIT,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::edit::line_col_to_byte;
use crate::error::AftError;
use crate::feature_status::{cause, IndexObservation};
use crate::inspect::job::is_test_file;
use crate::list_envelope::{ListEnvelope, Unit};
use crate::parser::{
    detect_language, extract_symbols_from_tree, grammar_for, FileParser, SharedSymbolCache,
};
use crate::protocol::Response;
use crate::symbols::Symbol;

pub type StoreAdapterResult<T> = Result<T, CallGraphStoreError>;

#[path = "../list_surfaces/callgraph.rs"]
pub mod callgraph_surface;
use callgraph_surface::*;

const TRACE_DATA_RESOLVER_PROVENANCE: &str = "treesitter+resolver";
pub const HUB_SUMMARY_THRESHOLD: usize = 20;
pub const HUB_SUMMARY_LIMIT: usize = 15;
// The agent only receives 15 representative paths once a trace becomes a hub. A
// 10k expansion budget leaves ample room for ordinary traces while preventing a
// layered call graph from unfolding millions of path prefixes synchronously.
const TRACE_TO_EXPANSION_BUDGET: usize = 10_000;
const TRACE_TO_RETAINED_PATH_LIMIT: usize = HUB_SUMMARY_LIMIT * 4;
/// Maximum number of macro-template and macro-invocation entries added to one
/// `callers` answer; further ones are not listed.
const MACRO_VIA_ENTRY_LIMIT: usize = 50;
/// Maximum number of unanalyzed macro mentions counted exactly in one
/// `callers` answer; beyond it the note reports the count as a lower bound.
const MACRO_MENTION_COUNT_LIMIT: usize = 100;
/// Maximum number of mention locations written out in the unanalyzed-macro
/// note; further locations are counted but not shown.
const MACRO_MENTION_SHOWN: usize = 5;

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

pub(crate) const CALLBACK_REFERENCE_LABEL: &str = " (callback reference)";
pub(crate) const UNRESOLVED_CALLERS_NOTE: &str = "Incomplete: {count} call site{plural} in the analyzed {language} code could not be resolved, so a dynamic or indirect call may still reach `{symbol}`.";
const TEST_FILTER_NOTE: &str = "Test filtering incomplete: AFT could not tell whether {count} caller site{plural} {verb} test code, so {subject} still shown.";

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
    #[serde(skip_serializing_if = "is_zero")]
    pub callback_dependencies: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_reference_label: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_filter_note: Option<String>,
    pub symbol: String,
    pub file: String,
    pub callers: Vec<StoreCallerGroup>,
    pub total_callers: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub hidden_test_callers: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub scanned_files: usize,
    pub depth_limited: bool,
    pub truncated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_list_envelope: Option<ListEnvelope>,
    /// Present when the target's name appears inside Rust macro token trees
    /// that could not be parsed, so the caller list may be incomplete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macro_note: Option<StoreMacroNote>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<StoreIncompleteNote>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreIncompleteNote {
    pub complete: bool,
    pub unresolved_call_sites: usize,
    pub incomplete_reason: String,
}

fn empty_receiver_note(
    store: &impl CallGraphRead,
    target: &StoreNode,
    count: usize,
    include_tests: bool,
) -> StoreAdapterResult<Option<StoreIncompleteNote>> {
    if count != 0 {
        return Ok(None);
    }
    let unresolved_call_sites = store.unresolved_call_site_count(target.lang, include_tests)?;
    let count_text = if unresolved_call_sites == UNRESOLVED_CALL_SITE_COUNT_LIMIT {
        format!("at least {unresolved_call_sites}")
    } else {
        unresolved_call_sites.to_string()
    };
    Ok((unresolved_call_sites > 0).then(|| StoreIncompleteNote {
        complete: false,
        unresolved_call_sites,
        incomplete_reason: UNRESOLVED_CALLERS_NOTE
            .replace("{count}", &count_text)
            .replace(
                "{plural}",
                if unresolved_call_sites == 1 { "" } else { "s" },
            )
            .replace("{language}", &format!("{:?}", target.lang))
            .replace("{symbol}", &target.name),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreMacroNote {
    pub message: String,
    /// Unanalyzed mention locations found (extraction keeps one per name and
    /// file), capped at `MACRO_MENTION_COUNT_LIMIT`.
    pub mentions: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub mentions_is_lower_bound: bool,
    /// `file:line` of the first mentions.
    pub sites: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallerGroup {
    pub file: String,
    pub callers: Vec<StoreCallerEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallerEntry {
    #[serde(skip_serializing_if = "is_false")]
    pub test_status_unknown: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<&'static str>,
    pub symbol: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    /// The macro (`name!`) whose template makes the call. Set on the sites
    /// that invoke that macro: the call runs there once the macro expands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
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
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub work_gap: Option<crate::callgraph::CallTreeWorkGap>,
    pub depth_limited: bool,
    pub truncated: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub hidden_test_callers: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_list_envelope: Option<ListEnvelope>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreImpactResult {
    #[serde(skip_serializing_if = "is_zero")]
    pub callback_dependencies: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_reference_label: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_filter_note: Option<String>,
    pub symbol: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub parameters: Vec<String>,
    pub total_affected: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub hidden_test_callers: usize,
    pub affected_files: usize,
    pub callers: Vec<StoreImpactCaller>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub depth_limited: bool,
    pub truncated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sites_list_envelope: Option<ListEnvelope>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<StoreIncompleteNote>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreImpactCaller {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<&'static str>,
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
    is_test_file(&site.caller.file) || site.caller_is_test == Some(true)
}

fn unknown_test_note(sites: &[StoreCallSite], include_tests: bool) -> Option<String> {
    test_filter_note_for_count(unknown_test_caller_count(sites), include_tests)
}

fn unknown_test_caller_count(sites: &[StoreCallSite]) -> usize {
    sites
        .iter()
        .filter(|site| {
            site.caller.lang == crate::parser::LangId::Rust
                && !is_test_file(&site.caller.file)
                && site.caller_is_test.is_none()
        })
        .count()
}

fn test_filter_note_for_count(count: usize, include_tests: bool) -> Option<String> {
    (!include_tests && count > 0).then(|| {
        TEST_FILTER_NOTE
            .replace("{count}", &count.to_string())
            .replace("{plural}", if count == 1 { "" } else { "s" })
            .replace("{verb}", if count == 1 { "is" } else { "are" })
            .replace("{subject}", if count == 1 { "it is" } else { "they are" })
    })
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

fn count_test_call_tree_nodes(node: &StoreCallTreeNode) -> usize {
    node.children
        .iter()
        .map(|child| usize::from(is_test_file(&child.file)) + count_test_call_tree_nodes(child))
        .sum()
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
    let mut visited = HashMap::new();
    let mut sites = Vec::new();
    let mut boundary = BTreeSet::new();
    let callers_by_symbol = prefetch_callers(
        store,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
    )?;

    collect_callers_recursive(
        &callers_by_symbol,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
        0,
        &mut visited,
        &mut sites,
        &mut boundary,
    )?;
    let (depth_limited, truncated) = omitted_boundary_callers(store, boundary, &visited)?;

    let mut sites = dedup_call_sites(sites);
    sites.sort_by(|left, right| {
        left.caller
            .file
            .cmp(&right.caller.file)
            .then(left.line.cmp(&right.line))
            .then(left.caller.symbol.cmp(&right.caller.symbol))
    });
    let pre_filter_count = sites.len();
    let unknown_test_callers = unknown_test_caller_count(&sites);
    let hidden_tests = sites
        .iter()
        .filter(|site| callsite_is_from_test(site))
        .count();
    let visible_sites = sites
        .into_iter()
        .filter(|site| include_tests || !callsite_is_from_test(site))
        .collect::<Vec<_>>();
    let post_filter_count = visible_sites.len();
    let callback_dependencies = visible_sites
        .iter()
        .filter(|site| site.kind == "callback_ref")
        .count();

    let summarize = hub_selector_activated(post_filter_count);
    let visible_sites = if summarize {
        dedup_sites_for_summary(visible_sites)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        visible_sites
    };
    let shown = visible_sites.len();

    // `total_callers` is a legacy JSON field consumers read as the pre-filter
    // count (hidden test callers included); it stays that way. The
    // requested-domain count the agent sees lives in the envelope, and the
    // text heading renders from the envelope when one is present.
    let total_callers = pre_filter_count;

    // Omit hub_summary when test filtering changes the caller count to avoid contradictory summary figures in the JSON output.
    let hub_summary = if summarize {
        if !include_tests && pre_filter_count != post_filter_count {
            None
        } else {
            Some(if include_tests {
                included_summary("callers", pre_filter_count, hidden_tests, shown)
            } else {
                test_hidden_summary("callers", pre_filter_count, hidden_tests, shown)
            })
        }
    } else {
        None
    };

    let mut groups: BTreeMap<String, Vec<StoreCallerEntry>> = BTreeMap::new();
    let mut listed = HashSet::new();
    for site in visible_sites {
        listed.insert((site.caller.file.clone(), site.line));
        groups
            .entry(site.caller.file.clone())
            .or_default()
            .push(StoreCallerEntry {
                test_status_unknown: site.caller.lang == crate::parser::LangId::Rust
                    && site.caller_is_test.is_none()
                    && !is_test_file(&site.caller.file),
                relation: (site.kind == "callback_ref").then_some("callback_reference"),
                symbol: site.caller.symbol.clone(),
                line: site.line,
                approximate: edge_approximate(&site),
                resolved_by: edge_resolved_by(&site),
                via: None,
            });
    }

    let is_rust = target.representative.lang == crate::parser::LangId::Rust;
    let via_entries = if is_rust {
        macro_via_entries(store, &target, include_tests, &listed)?
    } else {
        Vec::new()
    };
    let via_count = via_entries.len();
    let unknown_macro_callers = via_entries
        .iter()
        .filter(|(_, entry)| entry.test_status_unknown)
        .count();
    let test_filter_note =
        test_filter_note_for_count(unknown_test_callers + unknown_macro_callers, include_tests);
    for (file, entry) in via_entries {
        groups.entry(file).or_default().push(entry);
    }
    let macro_note = if is_rust {
        unanalyzed_macro_note(store, &target.representative.name, include_tests)?
    } else {
        None
    };

    let callers_list_envelope = build_callgraph_envelope(
        Unit::Items,
        shown + via_count,
        post_filter_count + via_count,
        truncated.max(usize::from(depth_limited)),
    );

    Ok(StoreCallersResult {
        callback_dependencies,
        callback_reference_label: (callback_dependencies > 0).then_some(CALLBACK_REFERENCE_LABEL),
        test_filter_note,
        incomplete: empty_receiver_note(
            store,
            &target.representative,
            post_filter_count + via_count,
            include_tests,
        )?,
        symbol: target.representative.symbol,
        file: target.representative.file,
        callers: groups
            .into_iter()
            .map(|(file, callers)| StoreCallerGroup { file, callers })
            .collect(),
        total_callers: total_callers + via_count,
        hidden_test_callers: if include_tests { 0 } else { hidden_tests },
        hub_summary,
        scanned_files: store.indexed_file_count()?,
        depth_limited,
        truncated,
        callers_list_envelope,
        macro_note,
    })
}

/// Callers reached through `macro_rules!` templates that call the target
/// outside any item the template defines. Each such template contributes its
/// own call site (listed under the macro name, `name!`) and every invocation
/// of the macro (listed under the invoking symbol, annotated `via name!`).
///
/// Templates, invocations and the target are all matched by name only, so
/// every entry is marked name-resolved. When several symbols share the
/// target's name no entries are produced, because nothing tells which of them
/// a template calls. Only macros defined in the project have template refs,
/// and the entry count is capped at `MACRO_VIA_ENTRY_LIMIT` by the queries.
fn macro_via_entries(
    store: &impl CallGraphRead,
    target: &ResolvedStoreSymbol,
    include_tests: bool,
    listed: &HashSet<(String, u32)>,
) -> StoreAdapterResult<Vec<(String, StoreCallerEntry)>> {
    let name = &target.representative.name;
    let template_calls =
        store.ref_sites_named(MACRO_BODY_CALL_REF_KIND, name, MACRO_VIA_ENTRY_LIMIT)?;
    if template_calls.is_empty() {
        return Ok(Vec::new());
    }
    let namesakes = store
        .nodes_matching(name)?
        .into_iter()
        .filter(|node| node.name == *name && !target.nodes.contains(node))
        .count();
    if namesakes > 0 {
        return Ok(Vec::new());
    }

    let name_match = |symbol: String, line: u32, via: Option<String>, test_status_unknown: bool| {
        StoreCallerEntry {
            test_status_unknown,
            relation: None,
            symbol,
            line,
            approximate: Some(true),
            resolved_by: Some("name_match".to_string()),
            via,
        }
    };
    let mut entries = Vec::new();
    let mut macros = BTreeSet::new();
    for site in template_calls {
        let Some(macro_name) = site.local_name else {
            continue;
        };
        if include_tests || !(is_test_file(&site.file) || site.is_test == Some(true)) {
            let unknown = site.is_test.is_none() && !is_test_file(&site.file);
            entries.push((
                site.file,
                name_match(macro_name.clone(), site.line, None, unknown),
            ));
        }
        macros.insert(macro_name);
    }
    for macro_name in macros {
        let remaining = MACRO_VIA_ENTRY_LIMIT.saturating_sub(entries.len());
        if remaining == 0 {
            break;
        }
        for site in store.ref_sites_named("call", &macro_name, remaining)? {
            if (!include_tests && (is_test_file(&site.file) || site.is_test == Some(true)))
                || listed.contains(&(site.file.clone(), site.line))
            {
                continue;
            }
            let symbol = site
                .caller_symbol
                .unwrap_or_else(|| "<top-level>".to_string());
            let unknown = site.is_test.is_none() && !is_test_file(&site.file);
            entries.push((
                site.file,
                name_match(symbol, site.line, Some(macro_name.clone()), unknown),
            ));
        }
    }
    Ok(entries)
}

/// A note naming where the target's name occurs inside Rust macro token trees
/// that could not be parsed as Rust. Calls there are invisible to the call
/// graph, so without the note an empty or short caller list would read as
/// complete when it may not be.
fn unanalyzed_macro_note(
    store: &impl CallGraphRead,
    name: &str,
    include_tests: bool,
) -> StoreAdapterResult<Option<StoreMacroNote>> {
    // One more than the limit tells an exact count from a lower bound.
    let rows =
        store.ref_sites_named(MACRO_MENTION_REF_KIND, name, MACRO_MENTION_COUNT_LIMIT + 1)?;
    let mut seen = BTreeSet::new();
    let mut sites = Vec::new();
    for row in rows {
        if !include_tests && is_test_file(&row.file) {
            continue;
        }
        if seen.insert((row.file.clone(), row.line)) {
            sites.push(format!("{}:{}", row.file, row.line));
        }
    }
    if sites.is_empty() {
        return Ok(None);
    }
    let mentions_is_lower_bound = sites.len() > MACRO_MENTION_COUNT_LIMIT;
    sites.truncate(MACRO_MENTION_COUNT_LIMIT);
    let mentions = sites.len();
    sites.truncate(MACRO_MENTION_SHOWN);
    let count = if mentions_is_lower_bound {
        format!("{mentions}+")
    } else {
        mentions.to_string()
    };
    let noun = if mentions == 1 { "mention" } else { "mentions" };
    let listed = if mentions > sites.len() {
        format!("{}, … (shown {} of {count})", sites.join(", "), sites.len())
    } else {
        sites.join(", ")
    };
    Ok(Some(StoreMacroNote {
        message: format!(
            "{count} {noun} of `{name}` inside macros could not be analyzed (one location per file): {listed}"
        ),
        mentions,
        mentions_is_lower_bound,
        sites,
    }))
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
    let mut memo = CallTreeMemo::default();
    let mut remaining = crate::callgraph::CALL_TREE_NODE_BUDGET;
    let mut tree = call_tree_inner(
        store,
        &target,
        depth.min(100),
        0,
        &mut visited,
        &mut memo,
        true,
        &mut remaining,
    )?;
    if remaining == 0 && tree.truncated > 0 {
        tree.work_gap = Some(crate::callgraph::CallTreeWorkGap {
            complete: false,
            nodes_examined: crate::callgraph::CALL_TREE_NODE_BUDGET,
            gap: crate::callgraph::CALL_TREE_WORK_GAP.into(),
        });
    }
    let hidden_test_callers = if include_tests {
        0
    } else {
        count_test_call_tree_nodes(&tree)
    };
    if !include_tests {
        filter_call_tree_tests(&mut tree);
    }
    let (shown, total_children) = cap_items(&mut tree.children);
    tree.hidden_test_callers = hidden_test_callers;
    tree.tree_list_envelope = build_callgraph_envelope(
        Unit::Items,
        shown,
        total_children,
        tree.truncated.max(usize::from(tree.depth_limited)),
    );
    if tree.work_gap.is_some() {
        tree.tree_list_envelope = Some(ListEnvelope::new(
            shown,
            crate::list_envelope::Total::AtLeast(total_children),
            Unit::Items,
            vec![crate::list_envelope::Reason::Cap],
            &["symbol", "depth"],
        ));
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
    let mut visited = HashMap::new();
    let mut sites = Vec::new();
    let mut boundary = BTreeSet::new();
    let callers_by_symbol = prefetch_callers(
        store,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
    )?;

    collect_callers_recursive(
        &callers_by_symbol,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
        0,
        &mut visited,
        &mut sites,
        &mut boundary,
    )?;
    let (depth_limited, truncated) = omitted_boundary_callers(store, boundary, &visited)?;

    let mut sites = dedup_call_sites(sites);
    sites.sort_by(|left, right| {
        left.caller
            .file
            .cmp(&right.caller.file)
            .then(left.line.cmp(&right.line))
            .then(left.caller.symbol.cmp(&right.caller.symbol))
    });
    let pre_filter_count = sites.len();
    let test_filter_note = unknown_test_note(&sites, include_tests);
    let hidden_tests = sites
        .iter()
        .filter(|site| callsite_is_from_test(site))
        .count();
    let visible_sites = sites
        .into_iter()
        .filter(|site| include_tests || !callsite_is_from_test(site))
        .collect::<Vec<_>>();
    let post_filter_count = visible_sites.len();
    let callback_dependencies = visible_sites
        .iter()
        .filter(|site| site.kind == "callback_ref")
        .count();

    let summarize = hub_selector_activated(post_filter_count);
    let affected_files = visible_sites
        .iter()
        .map(|site| site.caller.file.clone())
        .collect::<BTreeSet<_>>()
        .len();
    let visible_sites = if summarize {
        dedup_sites_for_summary(visible_sites)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        visible_sites
    };
    let shown = visible_sites.len();

    // `total_affected` is a legacy JSON field consumers read as the pre-filter
    // count; the requested-domain count lives in the envelope (see callers).
    let total_affected = pre_filter_count;

    // Omit hub_summary when test filtering changes the caller count to avoid contradictory summary figures in the JSON output.
    let hub_summary = if summarize {
        if !include_tests && pre_filter_count != post_filter_count {
            None
        } else {
            Some(if include_tests {
                included_summary("affected callers", pre_filter_count, hidden_tests, shown)
            } else {
                test_hidden_summary("affected callers", pre_filter_count, hidden_tests, shown)
            })
        }
    } else {
        None
    };

    let sites_list_envelope = build_callgraph_envelope(
        Unit::Sites,
        shown,
        post_filter_count,
        truncated.max(usize::from(depth_limited)),
    );
    let target_signature = target.representative.signature.clone();
    let target_parameters = target_signature
        .as_deref()
        .map(|signature| callgraph::extract_parameters(signature, target.representative.lang))
        .unwrap_or_default();

    let mut callers = Vec::new();
    // Each caller file is read once, however many of its call sites are shown,
    // and only up to the last shown line; only the shown lines are retained.
    let mut wanted_lines: HashMap<&str, BTreeSet<usize>> = HashMap::new();
    for site in &visible_sites {
        wanted_lines
            .entry(site.caller.file.as_str())
            .or_default()
            .insert(site.line.saturating_sub(1) as usize);
    }
    // Stored call positions are only meaningful for the content indexed.
    let mut stale_files = HashSet::new();
    let mut source_lines = HashMap::new();
    for (file, wanted) in wanted_lines {
        let path = store.project_root().join(file);
        if !file_matches_store(store, file, &path)? {
            stale_files.insert(file.to_string());
            continue;
        }
        source_lines.insert(file.to_string(), read_source_lines_at(&path, &wanted));
    }
    for site in visible_sites {
        let lines = source_lines.get(&site.caller.file).and_then(Option::as_ref);
        let stale = stale_files.contains(&site.caller.file);
        callers.push(StoreImpactCaller {
            relation: (site.kind == "callback_ref").then_some("callback_reference"),
            caller_symbol: site.caller.symbol.clone(),
            caller_file: site.caller.file.clone(),
            line: site.line,
            signature: site.caller.signature.clone(),
            is_entry_point: site.caller.is_entry_point,
            call_expression: lines
                .and_then(|lines| lines.get(&(site.line.saturating_sub(1) as usize)))
                .cloned(),
            parameters: site
                .caller
                .signature
                .as_deref()
                .map(|signature| callgraph::extract_parameters(signature, site.caller.lang))
                .unwrap_or_default(),
            approximate: if stale {
                Some(true)
            } else {
                edge_approximate(&site)
            },
            resolved_by: edge_resolved_by(&site),
        });
    }
    callers.sort_by(|left, right| {
        left.caller_file
            .cmp(&right.caller_file)
            .then(left.line.cmp(&right.line))
    });

    Ok(StoreImpactResult {
        callback_dependencies,
        callback_reference_label: (callback_dependencies > 0).then_some(CALLBACK_REFERENCE_LABEL),
        test_filter_note,
        incomplete: empty_receiver_note(
            store,
            &target.representative,
            post_filter_count,
            include_tests,
        )?,
        symbol: target.representative.symbol,
        file: target.representative.file,
        signature: target_signature,
        parameters: target_parameters,
        total_affected,
        hidden_test_callers: if include_tests { 0 } else { hidden_tests },
        affected_files,
        callers,
        hub_summary,
        depth_limited,
        truncated,
        sites_list_envelope,
    })
}

#[cfg(test)]
thread_local! {
    static TRACE_ENDPOINT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn trace_endpoint_key(node: &StoreNode) -> (String, String) {
    #[cfg(test)]
    TRACE_ENDPOINT_READS.with(|count| count.set(count.get() + 1));
    (node.file.clone(), node.symbol.clone())
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
    let mut pending_endpoints = BTreeSet::new();

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
        let caller_key = trace_endpoint_key(&current.node);
        if !callers_by_symbol.contains_key(&caller_key) {
            // Track unfetched endpoints when pushing paths, instead of
            // rescanning every pending path on each adjacency miss.
            let mut wanted = std::mem::take(&mut pending_endpoints);
            wanted.insert(caller_key.clone());
            let wanted = wanted.into_iter().collect::<Vec<_>>();
            let mut fetched = store.direct_callers_for_symbols(&wanted)?;
            for key in wanted {
                let callers = dedup_call_sites(
                    fetched
                        .remove(&key)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|site| site.kind == "call")
                        .collect(),
                );
                callers_by_symbol.insert(key, callers);
            }
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
            if depth + 1 < effective_max {
                let key = trace_endpoint_key(&site.caller);
                if !callers_by_symbol.contains_key(&key) {
                    pending_endpoints.insert(key);
                }
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
                // Hidden test callees would never be traversed, so they cannot
                // make an unfinished search incomplete either.
                if callees.iter().any(|(node, _)| {
                    (include_tests || !is_test_file(&node.file))
                        && !visited.contains(&(node.file.clone(), node.symbol.clone()))
                }) {
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
    let trace_files = TraceFileFacts::default();
    trace_data_inner(
        store,
        &symbol_cache,
        &trace_files,
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

/// Source, tree and symbols of each file one `trace_data` request walks.
/// Hops that return to a file (local calls, or several parameters flowing
/// into the same callee) reuse them instead of reading and parsing again.
#[derive(Default)]
struct TraceFileFacts {
    files: std::cell::RefCell<HashMap<PathBuf, Option<Rc<TraceFile>>>>,
}

struct TraceFile {
    source: String,
    content_hash: blake3::Hash,
    tree: tree_sitter::Tree,
    symbols: Vec<Symbol>,
}

#[cfg(test)]
thread_local! {
    pub(crate) static TRACE_FILE_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl TraceFileFacts {
    /// The parsed file, or None for a language without a grammar.
    fn load(&self, file: &Path) -> StoreAdapterResult<Option<Rc<TraceFile>>> {
        if let Some(cached) = self.files.borrow().get(file) {
            return Ok(cached.clone());
        }
        let source = std::fs::read_to_string(file)?;
        let parsed = match detect_language(file) {
            Some(lang) => {
                #[cfg(test)]
                TRACE_FILE_PARSES.with(|count| count.set(count.get() + 1));
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
                Some(Rc::new(TraceFile {
                    content_hash: crate::cache_freshness::hash_bytes(source.as_bytes()),
                    source,
                    tree,
                    symbols,
                }))
            }
            None => None,
        };
        self.files
            .borrow_mut()
            .insert(file.to_path_buf(), parsed.clone());
        Ok(parsed)
    }
}

#[allow(clippy::too_many_arguments)]
fn trace_data_inner(
    store: &impl CallGraphRead,
    symbol_cache: &SharedSymbolCache,
    files: &TraceFileFacts,
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
    let Some(facts) = files.load(file)? else {
        return Ok(());
    };
    let source = &facts.source;
    let tree = &facts.tree;
    let sym_info = facts
        .symbols
        .iter()
        .find(|candidate| {
            symbol_identity_from_cache(candidate) == symbol || candidate.name == symbol
        })
        .ok_or_else(|| CallGraphStoreError::StaleFiles(vec![rel_file.clone()]))?;

    let body_start = line_col_to_byte(source, sym_info.range.start_line, sym_info.range.start_col);
    let body_end = line_col_to_byte(source, sym_info.range.end_line, sym_info.range.end_col);
    let Some(body_node) = find_node_covering_range(tree.root_node(), body_start, body_end) else {
        return Ok(());
    };

    // An unindexed edit invalidates stored call offsets. Name-only matching
    // can still trace the current body, but cannot claim exact attribution.
    let stale = !content_matches_store(store, &rel_file, facts.content_hash)?;
    let current_calls = if stale { Vec::new() } else { current_calls };
    let mut tracked = TrackedBindings::with_origin(tracking_name, tracking_approximate || stale);
    walk_for_data_flow(
        store,
        symbol_cache,
        files,
        body_node,
        source,
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
    files: &TraceFileFacts,
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
            files,
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
                files,
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
    files: &TraceFileFacts,
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
            // Every followed hop, same-file ones included, spends depth;
            // otherwise a chain of local calls would ignore the limit.
            if current_depth + 1 > max_depth {
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
                    // The incoming hop uses the stored parameter name and location.
                    // Check the destination's parsed content before emitting that hop,
                    // not only when the recursive body walk starts afterwards.
                    let target_stale = match files.load(&target_file)? {
                        Some(facts) => {
                            !content_matches_store(store, &target.file, facts.content_hash)?
                        }
                        None => false,
                    };
                    let approximate = *approximate || target_stale;
                    hops.push(callgraph::DataFlowHop {
                        file: target.file.clone(),
                        symbol: target.symbol.clone(),
                        variable: param_name.clone(),
                        line: target.line,
                        flow_type: "parameter".to_string(),
                        approximate,
                    });
                    trace_data_inner(
                        store,
                        symbol_cache,
                        files,
                        &target_file,
                        &target.symbol,
                        param_name,
                        approximate,
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
    // A published WAL generation can still be busy during writer maintenance.
    // Contention is temporary, not evidence that the persisted graph is broken.
    if error.is_transient_lock_contention() {
        return Response::error(
            req_id,
            "callgraph_building",
            format!("{operation}: persisted callgraph store is busy; retry shortly"),
        );
    }
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

/// Do not turn missing coverage into a claim that a source symbol is absent.
/// Check the generation's file inventory before any graph or parser lookup.
pub fn ensure_target_indexed(
    req_id: &str,
    operation: &str,
    ctx: &AppContext,
    store: &crate::callgraph_store::ReadonlyCallGraphStore,
    file: &Path,
) -> Result<(), Response> {
    match store.is_file_indexed(file) {
        Ok(true) => return Ok(()),
        Err(error) => return Err(store_error_response(req_id, operation, error)),
        Ok(false) => {}
    }
    let path = absolute_file(store, file);
    let ignored = ctx.gitignore().is_some_and(|matcher| {
        matcher
            .matched_path_or_any_parents(&path, false)
            .is_ignore()
    });
    let (reason, detail) = if ignored {
        ("ignored", "path is ignored by the project's ignore rules")
    } else {
        ("not_in_generation", "path is not part of this call graph generation (excluded, unsupported, or not yet indexed)")
    };
    Err(Response::error_with_data(
        req_id,
        "not_indexed",
        format!("{operation}: not_indexed: {detail}: {}; use grep or aft_search with pattern to find references", path.display()),
        serde_json::json!({ "reason": reason, "file": path, "results": serde_json::Value::Null }),
    ))
}

#[cfg(test)]
mod serialization_tests {
    use super::*;

    #[test]
    fn sqlite_contention_is_retryable_but_other_store_errors_are_not() {
        for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
            let error = CallGraphStoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("database is locked".into()),
            ));
            let response = store_error_response("request", "trace_to", error);
            assert!(!response.success);
            assert_eq!(response.data["code"], "callgraph_building");
        }
        let response = store_error_response(
            "request",
            "trace_to",
            CallGraphStoreError::Sqlite(rusqlite::Error::InvalidQuery),
        );
        assert_eq!(response.data["code"], "callgraph_store_error");
    }

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

/// Refusal payload shared by every callgraph query whose index is not usable:
/// the analysis code plus the callgraph index status and cause, and a null
/// `results` so an unavailable graph is never read as an empty one.
fn callgraph_index_refusal(
    req_id: &str,
    code: &str,
    message: String,
    observation: IndexObservation,
) -> Response {
    Response::error_with_data(
        req_id,
        code,
        message,
        serde_json::json!({
            "index": { "callgraph": observation.consumer_json() },
            "results": serde_json::Value::Null,
        }),
    )
}

/// Refuse a callgraph query whose store is not ready. `Ready` and `Error`
/// are not refusals and must be handled by the caller; they fall back to the
/// unavailable refusal here only so the match stays total.
pub fn index_refusal_response(
    req_id: &str,
    operation: &str,
    ctx: &AppContext,
    access: &CallgraphStoreAccess,
) -> Response {
    match access {
        CallgraphStoreAccess::Off => off_response(req_id, operation),
        CallgraphStoreAccess::Building => {
            note_callgraph_building(ctx, operation);
            if let Some(view) = ctx.view_runtime_snapshot().filter(|view| {
                view.pending_inputs
                    .values()
                    .any(|input| input.plane == "checkout")
            }) {
                let mut response = callgraph_index_refusal(
                    req_id,
                    "callgraph_building",
                    format!("{operation}: call graph is waiting for working-tree verification and view publication; retry shortly"),
                    IndexObservation::building(),
                );
                response.data["progress"] = serde_json::json!({
                    "phase": "working_tree_refresh",
                    "pending_paths": view.pending_paths.len(),
                    "waiting_on": [{"plane": "checkout", "reason": "working-tree verification and view publication"}],
                });
                return response;
            }
            if ctx.config().views.enabled && ctx.is_worktree_bridge() {
                let view = ctx.view_runtime_snapshot();
                // Semantic fill is independent of callgraph publication. Only
                // report inputs that can actually prevent this graph's publish.
                let waiting_on = view.as_ref().map_or_else(Vec::new, |view| {
                    view.pending_inputs
                        .iter()
                        .filter(|(_, input)| input.plane != "semantic")
                        .map(|(path, input)| {
                            serde_json::json!({
                                "path": String::from_utf8_lossy(path),
                                "plane": input.plane,
                                "reason": input.reason,
                                "blob_key": input.blob_key,
                            })
                        })
                        .collect::<Vec<_>>()
                });
                let waiting_details = waiting_on
                    .iter()
                    .map(|input| {
                        format!(
                            "{} [{}: {}]",
                            input["path"].as_str().unwrap_or_default(),
                            input["plane"].as_str().unwrap_or_default(),
                            input["reason"].as_str().unwrap_or_default(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let message = if waiting_on.is_empty() {
                    format!("{operation}: call graph for this worktree is assembling; retry shortly; use grep or aft_search with pattern meanwhile")
                } else if waiting_on.iter().any(|input| input["plane"] == "callgraph") {
                    format!("{operation}: call graph for this worktree is waiting for shared blobs: {waiting_details}; full blob keys are in progress.waiting_on; an artifact writer must publish the missing blobs; retry after the inputs are available; use grep or aft_search with pattern meanwhile")
                } else {
                    format!("{operation}: call graph for this worktree is waiting for publication inputs: {waiting_details}; retry after the inputs are available; use grep or aft_search with pattern meanwhile")
                };
                let mut response = callgraph_index_refusal(
                    req_id,
                    "callgraph_building",
                    message,
                    IndexObservation::building(),
                );
                // Report the local view's phase and known pending count without
                // opening a store or enumerating checkout files on the query.
                response.data["progress"] = serde_json::json!({
                    "phase": if view.is_some() { "view_assembly" } else { "configure_maintenance" },
                    "pending_paths": view.as_ref().map(|view| if view.pending_inputs.is_empty() { view.pending_paths.len() } else { waiting_on.len() }),
                    "waiting_on": waiting_on,
                });
                response
            } else {
                building_response(req_id, operation)
            }
        }
        CallgraphStoreAccess::Suspended(suspension) => {
            suspended_response(req_id, operation, suspension)
        }
        CallgraphStoreAccess::Unavailable
        | CallgraphStoreAccess::Ready(_)
        | CallgraphStoreAccess::Error(_) => unavailable_for(req_id, operation, ctx),
    }
}

/// The callgraph index is turned off in the resolved configuration. Nothing
/// was started; the user can enable `indexes.callgraph` to use this query.
pub fn off_response(req_id: &str, operation: &str) -> Response {
    callgraph_index_refusal(
        req_id,
        "callgraph_off",
        format!("{operation}: the callgraph index is off (indexes.callgraph: false); enable it with aft setup to use callgraph queries"),
        IndexObservation::off(),
    )
}

/// The persisted callgraph store is cold-building in the background. The op did
/// not block the request thread; the agent should retry shortly. Mirrors how
/// semantic search reports a build in progress.
pub fn building_response(req_id: &str, operation: &str) -> Response {
    callgraph_index_refusal(
        req_id,
        "callgraph_building",
        format!("{operation}: callgraph store is building in the background; retry shortly"),
        IndexObservation::building(),
    )
}

/// A tripped breaker is terminal for this request: the store is configured,
/// but its builder was suspended. The index is unavailable with the
/// `build_suspended` cause; the message says how to resume.
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
    callgraph_index_refusal(
        req_id,
        "callgraph_unavailable",
        format!(
            "{operation}: build_suspended domain={} deaths={} age_ms={age_ms} reason={}; run doctor reset-build-breaker to resume",
            suspension.domain.as_str(),
            suspension.death_count,
            suspension.reason,
        ),
        IndexObservation::unavailable(cause::BUILD_SUSPENDED),
    )
}

/// Return the terminal navigation response for a HOME-root callgraph store.
///
/// HOME is configured successfully, but it is not a project root and must
/// never enter the cold-build/retry loop, so the index is unavailable with the
/// `home_root` cause rather than building.
pub fn home_root_disabled_response(req_id: &str, operation: &str) -> Response {
    callgraph_index_refusal(
        req_id,
        "callgraph_unavailable",
        format!(
            "{operation}: callgraph store is disabled for home roots; open a project subdirectory to enable it"
        ),
        IndexObservation::unavailable(cause::HOME_ROOT),
    )
}

pub fn unavailable_for(req_id: &str, operation: &str, ctx: &AppContext) -> Response {
    if let Some(reason) = ctx.cache_unavailable_reason() {
        return callgraph_index_refusal(
            req_id,
            crate::root_cache::CACHE_DIR_UNAVAILABLE,
            format!("{operation}: cache_dir_unavailable: {reason}"),
            IndexObservation::unavailable(crate::root_cache::CACHE_DIR_UNAVAILABLE),
        );
    }
    if ctx.is_home_root() {
        return home_root_disabled_response(req_id, operation);
    }
    if !ctx.is_worktree_bridge() && ctx.callgraph_project_root().is_some() {
        // A configured root whose store this runtime cannot use (for example
        // a borrowed read-only root, or a root that is being unbound): report
        // the plane's observed cause rather than "not configured".
        let observation = match crate::feature_status::observed_index_status(
            ctx,
            crate::feature_status::IndexPlane::Callgraph,
        ) {
            observation if observation.unavailable_reason.is_some() => observation,
            _ => IndexObservation::unavailable(cause::RUNTIME_NOT_OBSERVED),
        };
        return callgraph_index_refusal(
            req_id,
            "callgraph_unavailable",
            format!("{operation}: persisted callgraph store is unavailable in this runtime"),
            observation,
        );
    }
    unavailable_response(req_id, operation, ctx.is_worktree_bridge())
}

pub fn unavailable_response(req_id: &str, operation: &str, worktree: bool) -> Response {
    if worktree {
        return callgraph_index_refusal(
            req_id,
            "callgraph_unavailable",
            format!(
                "{operation}: no call graph has been built for this repository yet; it is built when the main checkout is opened; use grep or aft_search with pattern to find references in this checkout"
            ),
            IndexObservation::unavailable(cause::READ_ONLY_STORE_NOT_BUILT),
        );
    }
    Response::error(
        req_id,
        "not_configured",
        format!("{operation}: project not configured — send 'configure' first"),
    )
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
        .nodes_for_stored(file, symbol)?
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

/// Count callers omitted beyond the depth boundary once per symbol. A symbol
/// expanded through a shorter path has already contributed its callers.
fn omitted_boundary_callers(
    store: &impl CallGraphRead,
    mut boundary: BTreeSet<(String, String)>,
    expanded: &HashMap<(String, String), usize>,
) -> StoreAdapterResult<(bool, usize)> {
    boundary.retain(|key| !expanded.contains_key(key));
    if boundary.is_empty() {
        return Ok((false, 0));
    }
    let boundary = boundary.into_iter().collect::<Vec<_>>();
    let omitted = store
        .direct_caller_counts_of(&boundary)?
        .into_values()
        .fold(0usize, usize::saturating_add);
    Ok((omitted > 0, omitted))
}

#[allow(clippy::too_many_arguments)]
fn collect_callers_recursive(
    callers_by_symbol: &HashMap<(String, String), Vec<StoreCallSite>>,
    file: &str,
    symbol: &str,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashMap<(String, String), usize>,
    result: &mut Vec<StoreCallSite>,
    boundary: &mut BTreeSet<(String, String)>,
) -> StoreAdapterResult<()> {
    let key = (file.to_string(), symbol.to_string());
    if visited.get(&key).is_some_and(|best| *best <= current_depth) {
        return Ok(());
    }
    // A shallow revisit can reach callers omitted by an earlier deeper path.
    let first_visit = visited.insert(key, current_depth).is_none();

    let Some(sites) = callers_by_symbol.get(&(file.to_string(), symbol.to_string())) else {
        return Ok(());
    };
    for site in sites {
        if first_visit {
            result.push(site.clone());
        }
        if current_depth + 1 < max_depth {
            collect_callers_recursive(
                callers_by_symbol,
                &site.caller.file,
                &site.caller.symbol,
                max_depth,
                current_depth + 1,
                visited,
                result,
                boundary,
            )?;
        } else {
            boundary.insert((site.caller.file.clone(), site.caller.symbol.clone()));
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
    memo: &mut CallTreeMemo,
    memoize_adjacency: bool,
    remaining: &mut usize,
) -> StoreAdapterResult<StoreCallTreeNode> {
    *remaining -= 1;
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
            hidden_test_callers: 0,
            tree_list_envelope: None,
            work_gap: None,
        });
    }
    visited.insert(visit_key.clone());

    // Only adjacency rows are shared across converging paths. The visited set remains
    // path-local so repeated nodes still render independently with correct cycle guards.
    let calls = forward_calls_cached(store, current, &mut memo.adjacency, memoize_adjacency)?;
    let mut children = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;

    if current_depth < max_depth {
        for call in calls.iter() {
            if *remaining == 0 {
                truncated += 1;
                break;
            }
            match call {
                ForwardCall::Resolved(site) => {
                    let resolved = resolve_call_tree_target(store, site, memo, memoize_adjacency)?;
                    if let Some(child_symbol) = resolved {
                        let mut child = call_tree_inner(
                            store,
                            &child_symbol,
                            max_depth,
                            current_depth + 1,
                            visited,
                            memo,
                            memoize_adjacency,
                            remaining,
                        )?;
                        child.approximate = edge_approximate(site);
                        child.resolved_by = edge_resolved_by(site);
                        depth_limited |= child.depth_limited;
                        truncated += child.truncated;
                        children.push(child);
                    } else {
                        *remaining -= 1;
                        children.push(StoreCallTreeNode {
                            name: site.target_symbol.clone(),
                            file: site.target_file.clone(),
                            line: site.line,
                            signature: None,
                            resolved: false,
                            approximate: edge_approximate(site),
                            resolved_by: edge_resolved_by(site),
                            children: Vec::new(),
                            depth_limited: false,
                            truncated: 0,
                            hidden_test_callers: 0,
                            tree_list_envelope: None,
                            work_gap: None,
                        });
                    }
                }
                ForwardCall::Unresolved(call) => {
                    *remaining -= 1;
                    children.push(StoreCallTreeNode {
                        name: call.symbol.clone(),
                        file: call.caller.file.clone(),
                        line: call.line,
                        signature: None,
                        resolved: false,
                        approximate: None,
                        resolved_by: None,
                        children: Vec::new(),
                        depth_limited: false,
                        truncated: 0,
                        hidden_test_callers: 0,
                        tree_list_envelope: None,
                        work_gap: None,
                    });
                }
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
        hidden_test_callers: 0,
        tree_list_envelope: None,
        work_gap: None,
    })
}

/// Store reads shared by the converging paths of one call-tree request. The
/// rendered tree still walks every path; only adjacency rows and edge-target
/// resolutions are fetched once.
#[derive(Default)]
struct CallTreeMemo {
    adjacency: HashMap<(String, String), Rc<[ForwardCall]>>,
    targets: HashMap<(String, String, Option<String>), Option<Rc<ResolvedStoreSymbol>>>,
}

fn resolve_call_tree_target(
    store: &impl CallGraphRead,
    site: &StoreCallSite,
    memo: &mut CallTreeMemo,
    memoize: bool,
) -> StoreAdapterResult<Option<Rc<ResolvedStoreSymbol>>> {
    // The stored target node is part of the key: resolution falls back to it
    // when the exact lookup finds nothing.
    let key = (
        site.target_file.clone(),
        site.target_symbol.clone(),
        site.target.as_ref().map(|node| node.node_id().to_string()),
    );
    if memoize {
        if let Some(resolved) = memo.targets.get(&key) {
            return Ok(resolved.clone());
        }
    }
    let resolved = resolve_exact_symbol(
        store,
        &site.target_file,
        &site.target_symbol,
        site.target.clone(),
    )?
    .map(Rc::new);
    if memoize {
        memo.targets.insert(key, resolved.clone());
    }
    Ok(resolved)
}

fn forward_calls_cached(
    store: &impl CallGraphRead,
    current: &ResolvedStoreSymbol,
    cache: &mut HashMap<(String, String), Rc<[ForwardCall]>>,
    memoize: bool,
) -> StoreAdapterResult<Rc<[ForwardCall]>> {
    let key = (
        current.representative.file.clone(),
        current.representative.symbol.clone(),
    );
    if memoize {
        if let Some(calls) = cache.get(&key) {
            return Ok(Rc::clone(calls));
        }
    }
    let calls: Rc<[ForwardCall]> = forward_calls_for_nodes(store, &current.nodes)?.into();
    if memoize {
        cache.insert(key, Rc::clone(&calls));
    }
    Ok(calls)
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

fn call_site_key(site: &StoreCallSite) -> (String, u32, String, String, String) {
    (
        site.caller.file.clone(),
        site.line,
        site.target_file.clone(),
        site.target_symbol.clone(),
        site.kind.clone(),
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

/// Unknown or size-capped hashes cannot establish staleness. Otherwise stored
/// line and byte positions describe only the matching indexed content.
fn content_matches_store(
    store: &impl CallGraphRead,
    file: &str,
    current: blake3::Hash,
) -> StoreAdapterResult<bool> {
    let Some(stored) = store.stored_content_hash(file)? else {
        return Ok(true);
    };
    Ok(
        stored == crate::cache_freshness::zero_hash().to_hex().as_str()
            || stored == current.to_hex().as_str()
            || stored == view_blob_key(file, current).unwrap_or_default(),
    )
}

/// A view-materialized graph records each file's call-graph blob key rather
/// than its raw content hash. The key is derived from the content hash, the
/// file's language and the extractor version, so the current bytes match
/// exactly when they derive the stored key.
fn view_blob_key(file: &str, current: blake3::Hash) -> Option<String> {
    let language = if crate::views::assembly::is_resolution_input(file.as_bytes()) {
        "config".to_owned()
    } else {
        format!("{:?}", crate::parser::detect_language(Path::new(file))?).to_lowercase()
    };
    let key = crate::blob_store::CallgraphKey::from_source_digest(
        *current.as_bytes(),
        language,
        crate::views::callgraph::PRODUCER,
    )
    .full_key();
    Some(crate::blob_store::v2::FamilyKey::from(&key).to_hex())
}

fn file_matches_store(
    store: &impl CallGraphRead,
    file: &str,
    path: &Path,
) -> StoreAdapterResult<bool> {
    let Ok(mut reader) = std::fs::File::open(path) else {
        return Ok(true);
    };
    if reader
        .metadata()
        .is_ok_and(|metadata| metadata.len() > crate::cache_freshness::CONTENT_HASH_SIZE_CAP)
    {
        return Ok(true);
    }
    let mut hasher = blake3::Hasher::new();
    if std::io::copy(&mut reader, &mut hasher).is_err() {
        return Ok(true);
    }
    content_matches_store(store, file, hasher.finalize())
}

/// The requested zero-based lines of a file, trimmed. Reading stops after the
/// last requested line, so a large file is not loaded whole to show a few call
/// sites. Like a whole-file UTF-8 read, a decoding error in the lines read
/// yields None.
fn read_source_lines_at(path: &Path, wanted: &BTreeSet<usize>) -> Option<HashMap<usize, String>> {
    use std::io::BufRead;
    let last = *wanted.last()?;
    let reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut lines = HashMap::with_capacity(wanted.len());
    for (index, line) in reader.lines().enumerate() {
        if index > last {
            break;
        }
        let line = line.ok()?;
        if wanted.contains(&index) {
            lines.insert(index, line.trim().to_string());
        }
    }
    Some(lines)
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
        caller_count_identities: RefCell<Vec<(String, String)>>,
        node_lookups: RefCell<usize>,
        stored_hashes: HashMap<String, String>,
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
                caller_count_identities: RefCell::new(Vec::new()),
                node_lookups: RefCell::new(0),
                stored_hashes: HashMap::new(),
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

        fn add_outgoing(&mut self, caller: &StoreNode, target: &StoreNode) {
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
            self.caller_count_identities.borrow_mut().clear();
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
        let mut unused_cache = CallTreeMemo::default();
        let mut remaining = crate::callgraph::CALL_TREE_NODE_BUDGET;
        let mut uncached = call_tree_inner(
            &store,
            &resolved_root,
            3,
            0,
            &mut visited,
            &mut unused_cache,
            false,
            &mut remaining,
        )
        .expect("uncached call tree");
        let uncached_queries = store.total_forward_queries();

        let (uncached_shown, uncached_total) = cap_items(&mut uncached.children);
        uncached.tree_list_envelope = build_callgraph_envelope(
            Unit::Items,
            uncached_shown,
            uncached_total,
            uncached.truncated,
        );

        assert_eq!(call_tree_node_count(&memoized), 46);
        assert_eq!(
            serde_json::to_vec(&memoized).expect("serialize memoized tree"),
            serde_json::to_vec(&uncached).expect("serialize uncached tree"),
            "adjacency memoization must not change rendered call-tree bytes"
        );
        assert_eq!(uncached_queries, 1_202);
        assert_eq!(memoized_queries, 406);
    }

    #[test]
    fn call_tree_resolves_each_shared_edge_target_once() {
        let (store, root) = converging_call_tree_store(200);
        let resolved_root = ResolvedStoreSymbol {
            representative: root.clone(),
            nodes: vec![root.clone()],
        };
        let lookups_for = |memoize: bool| {
            *store.node_lookups.borrow_mut() = 0;
            let mut remaining = crate::callgraph::CALL_TREE_NODE_BUDGET;
            call_tree_inner(
                &store,
                &resolved_root,
                3,
                0,
                &mut HashSet::new(),
                &mut CallTreeMemo::default(),
                memoize,
                &mut remaining,
            )
            .expect("call tree");
            *store.node_lookups.borrow()
        };
        // 200 handlers, each reached once, all calling the same helper, which
        // calls the same leaf: 200 + 200 + 200 lookups without the memo.
        assert_eq!(lookups_for(false), 600);
        // With it: each handler once, plus helper and leaf once each.
        assert_eq!(lookups_for(true), 202);
    }

    #[test]
    fn impact_does_not_show_call_text_from_a_caller_edited_since_indexing() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = CountingStore::new();
        store.root = temp.path().to_path_buf();
        let target = node("target", false);
        let mut caller = node("caller", false);
        caller.line = 2;
        store.add_node(target.clone());
        store.add_node(caller.clone());
        store.add_caller_at(&target, &caller, 2);
        let indexed = "function caller() {\n  target();\n}\n";
        std::fs::write(temp.path().join(&caller.file), indexed).unwrap();
        store.stored_hashes.insert(
            caller.file.clone(),
            crate::cache_freshness::hash_bytes(indexed.as_bytes())
                .to_hex()
                .to_string(),
        );
        let fresh =
            impact_result(&store, Path::new(&target.file), &target.symbol, 1, true).unwrap();
        assert_eq!(
            fresh.callers[0].call_expression.as_deref(),
            Some("target();")
        );
        assert_eq!(fresh.callers[0].approximate, None);
        // The unindexed edit moves the call off its stored line.
        std::fs::write(
            temp.path().join(&caller.file),
            "function caller() {\n  // moved\n  target();\n}\n",
        )
        .unwrap();
        let stale =
            impact_result(&store, Path::new(&target.file), &target.symbol, 1, true).unwrap();
        assert_eq!(stale.callers[0].call_expression, None);
        assert_eq!(stale.callers[0].approximate, Some(true));
    }

    fn trace_data_across_files_with_unindexed_edits(
        edit_source: bool,
        edit_destination: bool,
    ) -> callgraph::TraceDataResult {
        let temp = tempfile::tempdir().unwrap();
        let mut store = CountingStore::new();
        store.root = temp.path().canonicalize().unwrap();
        let source_text = "function entry(x: number) { sink(x); }\n";
        let destination_text = "function sink(p: number) { const copied = p; }\n";
        let mut source = node("entry", false);
        source.signature = Some("function entry(x: number)".into());
        let mut destination = node("sink", false);
        destination.signature = Some("function sink(p: number)".into());
        for (node, text) in [(&source, source_text), (&destination, destination_text)] {
            store.add_node(node.clone());
            std::fs::write(store.root.join(&node.file), text).unwrap();
            store.stored_hashes.insert(
                node.file.clone(),
                crate::cache_freshness::hash_bytes(text.as_bytes())
                    .to_hex()
                    .to_string(),
            );
        }
        store.add_outgoing(&source, &destination);
        let site = &mut store
            .outgoing
            .get_mut(&(source.file.clone(), source.symbol.clone()))
            .unwrap()[0];
        site.byte_start = source_text.find("sink(x)").unwrap();
        site.byte_end = site.byte_start + "sink(x)".len();
        if edit_source {
            std::fs::write(
                store.root.join(&source.file),
                format!("// source shifted\n{source_text}"),
            )
            .unwrap();
        }
        if edit_destination {
            std::fs::write(
                store.root.join(&destination.file),
                format!("// destination shifted\n{destination_text}"),
            )
            .unwrap();
        }
        trace_data_result(
            &store,
            &store.root.join(&source.file),
            "entry",
            "x",
            5,
            std::sync::Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        )
        .unwrap()
    }

    #[test]
    fn trace_data_destination_only_edit_marks_the_first_parameter_hop_approximate() {
        let trace = trace_data_across_files_with_unindexed_edits(false, true);
        let first = trace.hops.first().expect("parameter hop into destination");
        assert_eq!(first.flow_type, "parameter");
        assert_eq!(first.file, "sink.ts");
        assert_eq!(first.variable, "p");
        assert_eq!(
            first.line, 1,
            "the hop still uses the stored parameter location"
        );
        assert!(first.approximate, "{first:?}");
        assert!(trace
            .hops
            .iter()
            .any(|hop| hop.variable == "copied" && hop.approximate));
    }

    #[test]
    fn trace_data_source_only_edit_keeps_the_first_parameter_hop_approximate() {
        let trace = trace_data_across_files_with_unindexed_edits(true, false);
        let first = trace.hops.first().expect("name-only parameter hop");
        assert_eq!(first.flow_type, "parameter");
        assert_eq!(first.file, "entry.ts");
        assert_eq!(first.variable, "x");
        assert!(first.approximate, "{first:?}");
    }

    #[test]
    fn trace_data_unchanged_files_keep_the_first_parameter_hop_exact() {
        let trace = trace_data_across_files_with_unindexed_edits(false, false);
        let first = trace.hops.first().expect("parameter hop into destination");
        assert_eq!(first.flow_type, "parameter");
        assert_eq!(first.file, "sink.ts");
        assert_eq!(first.variable, "p");
        assert!(!first.approximate, "{first:?}");
        assert!(trace
            .hops
            .iter()
            .any(|hop| hop.variable == "copied" && !hop.approximate));
    }

    #[test]
    fn impact_source_lines_stop_after_the_last_shown_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("caller.ts");
        let mut bytes = b"first()\n   second();  \nthird()\n".to_vec();
        // Bytes past the last shown line are never read, so an undecodable
        // tail no longer hides the call expressions above it.
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
        std::fs::write(&path, bytes).unwrap();
        let lines = read_source_lines_at(&path, &BTreeSet::from([1])).unwrap();
        assert_eq!(lines, HashMap::from([(1, "second();".to_string())]));
        assert_eq!(read_source_lines_at(&path, &BTreeSet::from([3])), None);
    }

    #[test]
    fn call_tree_adjacency_hits_share_the_same_allocation() {
        let (store, root) = converging_call_tree_store(2);
        let current = ResolvedStoreSymbol {
            representative: root.clone(),
            nodes: vec![root],
        };
        let mut cache = HashMap::new();
        let first = forward_calls_cached(&store, &current, &mut cache, true).unwrap();
        let second = forward_calls_cached(&store, &current, &mut cache, true).unwrap();
        assert!(!first.is_empty());
        assert!(
            Rc::ptr_eq(&first, &second),
            "an adjacency hit must not deep-clone rows"
        );
    }

    #[test]
    fn trace_pending_endpoint_index_bounds_path_end_reads() {
        let mut store = CountingStore::new();
        let target = node("target", false);
        store.add_node(target.clone());
        let mut branch = target.clone();
        for index in 0..200 {
            let caller = node(&format!("wide_{index}"), false);
            store.add_node(caller.clone());
            store.add_caller(&target, &caller);
            branch = caller;
        }
        for index in 0..12 {
            let caller = node(&format!("deep_{index}"), index == 11);
            store.add_node(caller.clone());
            store.add_caller(&branch, &caller);
            branch = caller;
        }
        TRACE_ENDPOINT_READS.with(|count| count.set(0));
        let (result, expansions) = trace_to_result_with_budget(
            &store,
            Path::new(&target.file),
            &target.symbol,
            20,
            true,
            1_000,
        )
        .unwrap();
        assert_eq!(result.total_paths, 1);
        TRACE_ENDPOINT_READS.with(|count| {
            assert!(
                count.get() <= expansions * 2 + 1,
                "{} endpoint reads for {expansions} expansions",
                count.get()
            )
        });
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
    fn reverse_replies_batch_all_boundary_symbols_once() {
        for width in [2, 20] {
            for depth in [1, 5, 100] {
                let temp = tempfile::tempdir().unwrap();
                let (mut store, target) = layered_store(width, depth + 1);
                store.root = temp.path().to_path_buf();
                let first = store.nodes[&("layer_1_0.ts".into(), "layer_1_0".into())].clone();
                // The layered fanout supplies diamond joins; this back edge
                // makes revisiting an already expanded symbol unavoidable.
                store.add_caller(&first, &target);
                for include_tests in [false, true] {
                    for op in ["callers", "impact"] {
                        store.reset_query_counts();
                        let (depth_limited, truncated) = if op == "callers" {
                            let reply = callers_result(
                                &store,
                                Path::new(&target.file),
                                &target.symbol,
                                depth,
                                include_tests,
                            )
                            .unwrap();
                            (reply.depth_limited, reply.truncated)
                        } else {
                            let reply = impact_result(
                                &store,
                                Path::new(&target.file),
                                &target.symbol,
                                depth,
                                include_tests,
                            )
                            .unwrap();
                            (reply.depth_limited, reply.truncated)
                        };
                        assert!(depth_limited);
                        // At depth one this back edge lies beyond the boundary,
                        // so it adds one to the omitted count.
                        assert_eq!(truncated, width * width + usize::from(depth == 1));
                        assert_eq!(store.total_caller_queries(), 0);
                        assert_eq!(store.total_caller_frontier_queries(), depth);
                        assert_eq!(
                            store.caller_frontier_target_count(),
                            1 + width * (depth - 1)
                        );
                        assert_eq!(
                            store.total_caller_count_queries(),
                            1,
                            "{op}: width={width}, depth={depth}, include_tests={include_tests}"
                        );
                        assert_eq!(store.caller_count_target_count(), width);
                        let identities = store.caller_count_identities.borrow();
                        let actual: BTreeSet<_> = identities.iter().cloned().collect();
                        let expected: BTreeSet<_> = (0..width)
                            .map(|index| {
                                (
                                    format!("layer_{depth}_{index}.ts"),
                                    format!("layer_{depth}_{index}"),
                                )
                            })
                            .collect();
                        assert_eq!(
                            actual, expected,
                            "{op}: boundary identities at depth {depth}"
                        );
                        assert_eq!(
                            identities.len(),
                            actual.len(),
                            "{op}: duplicate boundary identity"
                        );
                        println!(
                            "{op}: width={width}, depth={depth}, include_tests={include_tests}: boundary_calls=1 boundary_targets={width} frontier_calls={depth} omitted={truncated}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn reverse_replies_preserve_diamond_cycle_and_hidden_test_json() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = CountingStore::new();
        store.root = temp.path().to_path_buf();
        let target = node("target", false);
        let alpha = node("alpha", false);
        let beta = node("beta", false);
        let shared = node("shared", false);
        let test = StoreNode::for_test("shared.test.ts", "test", true);
        let entry = node("entry", true);
        for item in [&target, &alpha, &beta, &shared, &test, &entry] {
            store.add_node(item.clone());
        }
        // Reverse insertion order and a duplicate exercise output sorting and
        // site deduplication independently of traversal order.
        store.add_caller_at(&target, &beta, 3);
        store.add_caller_at(&target, &alpha, 2);
        store.add_caller_at(&target, &alpha, 2);
        store.add_caller_at(&alpha, &shared, 4);
        store.add_caller_at(&beta, &shared, 4);
        store.add_caller_at(&shared, &target, 1);
        store.add_caller_at(&shared, &entry, 5);
        store.add_caller_at(&alpha, &test, 6);
        store.add_caller_at(&test, &entry, 5);

        let callers = callers_result(&store, Path::new(&target.file), &target.symbol, 2, false)
            .expect("callers result");
        assert_eq!(
            serde_json::to_string(&callers).unwrap(),
            r#"{"symbol":"target","file":"target.ts","callers":[{"file":"alpha.ts","callers":[{"symbol":"alpha","line":2}]},{"file":"beta.ts","callers":[{"symbol":"beta","line":3}]},{"file":"shared.ts","callers":[{"symbol":"shared","line":4},{"symbol":"shared","line":4}]}],"total_callers":5,"hidden_test_callers":1,"scanned_files":6,"depth_limited":true,"truncated":3,"callers_list_envelope":{"shown":4,"total":{"kind":"at_least","value":4},"unit":"items","reason":"depth","causes":["depth"],"narrow":["depth","includeTests"]}}"#
        );
        let impact = impact_result(&store, Path::new(&target.file), &target.symbol, 2, false)
            .expect("impact result");
        assert_eq!(
            serde_json::to_string(&impact).unwrap(),
            r#"{"symbol":"target","file":"target.ts","parameters":[],"total_affected":5,"hidden_test_callers":1,"affected_files":3,"callers":[{"caller_symbol":"alpha","caller_file":"alpha.ts","line":2,"is_entry_point":false,"parameters":[]},{"caller_symbol":"beta","caller_file":"beta.ts","line":3,"is_entry_point":false,"parameters":[]},{"caller_symbol":"shared","caller_file":"shared.ts","line":4,"is_entry_point":false,"parameters":[]},{"caller_symbol":"shared","caller_file":"shared.ts","line":4,"is_entry_point":false,"parameters":[]}],"depth_limited":true,"truncated":3,"sites_list_envelope":{"shown":4,"total":{"kind":"at_least","value":4},"unit":"sites","reason":"depth","causes":["depth"],"narrow":["depth","includeTests"]}}"#
        );
    }

    #[test]
    fn callers_reexpand_a_symbol_reached_later_at_a_shallower_depth() {
        let mut store = CountingStore::new();
        let target = node("target", false);
        let deep = node("deep", false);
        let middle = node("middle", false);
        let shared = node("shared", false);
        let upstream = node("upstream", false);
        let entry = node("entry", true);
        for item in [&target, &deep, &middle, &shared, &upstream, &entry] {
            store.add_node(item.clone());
        }
        store.add_caller(&target, &deep);
        store.add_caller(&deep, &middle);
        store.add_caller(&middle, &shared);
        store.add_caller(&target, &shared);
        store.add_caller(&shared, &upstream);
        store.add_caller(&upstream, &entry);
        let result =
            callers_result(&store, Path::new(&target.file), &target.symbol, 4, true).unwrap();
        assert!(result.callers.iter().any(|group| group.file == entry.file));
    }

    #[test]
    fn callers_and_impact_count_callers_omitted_at_the_depth_limit() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = CountingStore::new();
        store.root = temp.path().to_path_buf();
        let target = node("target", false);
        let boundary = node("boundary", false);
        let upstream_a = node("upstreamA", true);
        let upstream_b = node("upstreamB", true);
        for fixture_node in [&target, &boundary, &upstream_a, &upstream_b] {
            store.add_node(fixture_node.clone());
        }
        store.add_caller(&target, &boundary);
        store.add_caller(&boundary, &upstream_a);
        store.add_caller(&boundary, &upstream_b);
        let callers =
            callers_result(&store, Path::new(&target.file), &target.symbol, 1, true).unwrap();
        assert!(callers.depth_limited);
        assert_eq!(callers.truncated, 2);
        let impact =
            impact_result(&store, Path::new(&target.file), &target.symbol, 1, true).unwrap();
        assert!(impact.depth_limited);
        assert_eq!(impact.truncated, 2);
        // At depth two, upstreamA and upstreamB are both listed.
        let deeper =
            callers_result(&store, Path::new(&target.file), &target.symbol, 2, true).unwrap();
        assert!(!deeper.depth_limited);
        assert_eq!(deeper.truncated, 0);
    }

    #[test]
    fn boundary_callers_expanded_by_a_shorter_path_are_not_counted_as_omitted() {
        let mut store = CountingStore::new();
        let target = node("target", false);
        let deep = node("deep", false);
        let shared = node("shared", false);
        let entry = node("entry", true);
        for fixture_node in [&target, &deep, &shared, &entry] {
            store.add_node(fixture_node.clone());
        }
        store.add_caller(&target, &deep);
        store.add_caller(&deep, &shared);
        store.add_caller(&target, &shared);
        store.add_caller(&shared, &entry);
        let result =
            callers_result(&store, Path::new(&target.file), &target.symbol, 2, true).unwrap();
        assert!(result.callers.iter().any(|group| group.file == entry.file));
        assert!(!result.depth_limited, "every caller was listed");
        assert_eq!(result.truncated, 0);
    }

    #[test]
    // At depth one hubCaller is listed, but upstreamA and upstreamB are not.
    fn batched_boundary_counts_preserve_serialized_callers_and_impact_contract() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = CountingStore::new();
        store.root = temp.path().to_path_buf();
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
            r#"{"symbol":"target","file":"target.ts","callers":[{"file":"hubCaller.ts","callers":[{"symbol":"hubCaller","line":1}]}],"total_callers":21,"hub_summary":{"message":"Next: 21 callers — showing 1; narrow with scope","total":21,"hidden_tests":0,"shown":1,"threshold":20,"limit":15},"scanned_files":4,"depth_limited":true,"truncated":2,"callers_list_envelope":{"shown":1,"total":{"kind":"at_least","value":1},"unit":"items","reason":"depth","causes":["depth","cap"],"narrow":["depth","includeTests"]}}"#
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
            r#"{"symbol":"target","file":"target.ts","parameters":[],"total_affected":21,"affected_files":1,"callers":[{"caller_symbol":"hubCaller","caller_file":"hubCaller.ts","line":1,"is_entry_point":false,"parameters":[]}],"hub_summary":{"message":"Next: 21 affected callers — showing 1; narrow with scope","total":21,"hidden_tests":0,"shown":1,"threshold":20,"limit":15},"depth_limited":true,"truncated":2,"sites_list_envelope":{"shown":1,"total":{"kind":"at_least","value":1},"unit":"sites","reason":"depth","causes":["depth","cap"],"narrow":["depth","includeTests"]}}"#
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
    fn trace_to_symbol_ignores_hidden_test_callees_at_the_depth_limit() {
        let mut store = CountingStore::new();
        let origin = node("origin", false);
        let leaf = node("leaf", false);
        let helper = StoreNode::for_test("leaf.test.ts", "testHelper", false);
        for fixture_node in [&origin, &leaf, &helper] {
            store.add_node(fixture_node.clone());
        }
        store.add_outgoing(&origin, &leaf);
        store.add_outgoing(&leaf, &helper);

        let hidden = trace_to_symbol_result(
            &store,
            Path::new(&origin.file),
            &origin.symbol,
            "missing",
            None,
            1,
            false,
        )
        .expect("trace-to-symbol result");
        assert!(
            hidden.complete,
            "only a hidden test callee lies past the limit"
        );
        assert_eq!(hidden.reason.as_deref(), Some("no_path_found"));

        let included = trace_to_symbol_result(
            &store,
            Path::new(&origin.file),
            &origin.symbol,
            "missing",
            None,
            1,
            true,
        )
        .expect("trace-to-symbol result");
        assert!(!included.complete);
        assert_eq!(included.reason.as_deref(), Some("max_depth_exhausted"));
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
        // Callers are fetched in frontier batches; summed over the batches,
        // each of the 7 distinct symbols is requested exactly once.
        assert_eq!(store.total_caller_queries(), 0);
        assert_eq!(store.caller_frontier_target_count(), 7);
        assert!(
            store.total_caller_frontier_queries() < 7,
            "callers must be fetched in batches, not once per symbol: {} queries",
            store.total_caller_frontier_queries()
        );
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

#[cfg(test)]
#[path = "callgraph_regression_tests.rs"]
mod callgraph_regression_tests;
