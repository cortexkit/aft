//! Query fan-out for parent folder sessions: each request reads the warm
//! child planes the worker installed, merges their answers under the parent
//! root, and names every child (or area outside the children) it could not
//! answer for.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::commands::callgraph_store_adapter as adapter;
use crate::context::AppContext;
use crate::grep_executor::{GrepExecutionPhaseTimings, GrepParams, GrepScope};
use crate::pattern_compile::CompiledPattern;
use crate::protocol::{RawRequest, Response};
use crate::search_index::{GrepResult, IndexStatus, PathFilters, SearchIndexSnapshot};

use super::{display_relative, Child, ParentSession, Plane};

/// Commands a parent session answers itself from its children.
const ROUTED_COMMANDS: &[&str] = &[
    "semantic_search",
    "callers",
    "call_tree",
    "impact",
    "trace_to",
    "trace_to_symbol",
    "trace_data",
    "inspect",
];

/// Named gaps collected while answering one request, deduplicated.
#[derive(Default)]
pub(crate) struct Gaps(Vec<Value>);

impl Gaps {
    pub(crate) fn push(&mut self, kind: &str, path: String, reason: String) {
        let gap = json!({"kind": kind, "path": path, "reason": reason});
        if !self.0.contains(&gap) {
            self.0.push(gap);
        }
    }

    /// Records that `child` cannot answer this request, and why.
    pub(crate) fn child(&mut self, child: &Child, reason: String) {
        self.push("parent_child_unavailable", child.display(), reason);
    }

    /// Children, skipped children and outside paths a search of `scope`
    /// covers but cannot reach, beyond the per-child plane gaps.
    pub(crate) fn scope(&mut self, session: &ParentSession, scope: &Path) {
        let Some(discovery) = session.discovery() else {
            self.discovering();
            return;
        };
        let in_scope = |path: &Path| path.starts_with(scope) || scope.starts_with(path);
        let skipped = discovery
            .skipped
            .iter()
            .filter(|path| in_scope(path))
            .map(|path| display_relative(&discovery.root, path))
            .collect::<Vec<_>>();
        if !skipped.is_empty() {
            self.push(
                "parent_children_over_cap",
                display_relative(&discovery.root, scope),
                format!(
                    "{} child repositories beyond the {}-repository cap were not searched: {}",
                    skipped.len(),
                    discovery.children.len(),
                    skipped.join(", ")
                ),
            );
        }
        for outside in discovery.outside.iter().filter(|path| in_scope(path)) {
            self.push(
                "outside_child_repositories",
                display_relative(&discovery.root, outside),
                "holds files outside every child repository; a parent folder searches only its repositories"
                    .into(),
            );
        }
    }

    /// The worker has not finished finding the child repositories yet.
    pub(crate) fn discovering(&mut self) {
        self.push(
            "parent_discovering",
            ".".into(),
            "still discovering child repositories; their answers are not included yet".into(),
        );
    }

    pub(crate) fn into_values(self) -> Vec<Value> {
        self.0
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Marks `body` incomplete and lists `gaps` in its `gaps` field and text.
pub(crate) fn attach_gaps(body: &mut Value, gaps: Vec<Value>) {
    if gaps.is_empty() {
        return;
    }
    let lines = gaps
        .iter()
        .map(|gap| {
            format!(
                "- {}: {}",
                gap["path"].as_str().unwrap_or("."),
                gap["reason"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(object) = body.as_object_mut() {
        object.insert("complete".into(), Value::Bool(false));
        let entry = object
            .entry("gaps")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(existing) = entry.as_array_mut() {
            for gap in gaps {
                if !existing.contains(&gap) {
                    existing.push(gap);
                }
            }
        }
        if let Some(Value::String(text)) = object.get_mut("text") {
            text.push_str(&format!(
                "\n\n(Parent folder: not covered by this answer:\n{lines})"
            ));
        }
    }
}

/// True once discovery finished; otherwise records the discovering gap.
pub(crate) fn discovered(session: &ParentSession, gaps: &mut Gaps) -> bool {
    if session.discovery().is_some() {
        return true;
    }
    gaps.discovering();
    false
}

/// Children whose checkout overlaps `scope`: inside it, or containing it.
fn overlapping(session: &ParentSession, scope: &Path) -> Vec<Arc<Child>> {
    session
        .children()
        .into_iter()
        .filter(|child| child.root.starts_with(scope) || scope.starts_with(&child.root))
        .collect()
}

/// One child's text index as grep and glob read it: the child's checkout in
/// the index store, scoped to the part of the search root inside the child.
enum ChildText {
    Data(crate::data_layer::tools::grep::ViewScope),
}

impl ChildText {
    /// Whether the child holds any file under `search_root`.
    fn has_files(&self, search_root: &Path) -> bool {
        let _ = search_root;
        match self {
            Self::Data(scope) => scope.has_files(),
        }
    }

    /// Whether the child holds any file under `search_root` that passes
    /// `filters`, which match paths relative to the child's root.
    fn has_files_matching(&self, search_root: &Path, filters: &PathFilters) -> bool {
        let _ = search_root;
        match self {
            Self::Data(scope) => scope.has_files_matching(filters),
        }
    }

    /// A snapshot holding every file of the child that may match `pattern`.
    fn candidates(
        &self,
        pattern: &CompiledPattern,
    ) -> Result<Cow<'_, SearchIndexSnapshot>, String> {
        match self {
            Self::Data(scope) => scope
                .candidates(pattern)
                .map(Cow::Owned)
                .map_err(|error| error.to_string()),
        }
    }

    /// A snapshot listing the child's files, for glob.
    fn listing(&self) -> Cow<'_, SearchIndexSnapshot> {
        match self {
            Self::Data(scope) => Cow::Owned(scope.listing()),
        }
    }
}

/// Each child overlapping `search_root`, with its text index (the child's
/// own checkout in the index store) or the reason it cannot answer.
fn child_texts(
    ctx: &AppContext,
    session: &ParentSession,
    search_root: &Path,
) -> Vec<(Arc<Child>, Result<ChildText, String>)> {
    let children = overlapping(session, search_root);
    // A plane the user turned off is a named gap for every child.
    if !session.planes.trigram {
        return children
            .into_iter()
            .map(|child| (child, Err("trigram: the trigram index is off".to_string())))
            .collect();
    }
    let roots = children
        .iter()
        .map(|child| child.root.as_path())
        .collect::<Vec<_>>();
    let planes = crate::data_layer::parent::text_planes(ctx, session.root(), &roots);
    children
        .into_iter()
        .zip(planes)
        .filter_map(|(child, plane)| {
            let text = match plane {
                Plane::Ready(view) => {
                    // The whole child when the search root holds it.
                    let scope = if search_root.starts_with(&child.root) {
                        search_root
                    } else {
                        child.root.as_path()
                    };
                    Ok(ChildText::Data(
                        crate::data_layer::tools::grep::ViewScope::of((*view).clone(), scope)?,
                    ))
                }
                plane => Err(plane.gap_reason("trigram")?),
            };
            Some((child, text))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// grep and glob
// ---------------------------------------------------------------------------

/// The parent session's grep: every directory root is answered from the
/// warm trigram snapshots of the children it overlaps. Explicit files and
/// roots outside the parent folder keep the ordinary executor, which reads
/// them directly. `None` when `ctx` is not a parent session.
pub(crate) fn grep_fan_out(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    scope: &GrepScope,
    params: &GrepParams,
    filters: &PathFilters,
    order: crate::search_index::GrepScanOrder,
) -> Option<(GrepResult, GrepExecutionPhaseTimings, Vec<Value>)> {
    let session = super::session_for(ctx)?;
    let project_root = crate::grep_executor::project_root(ctx);
    let query = crate::search_index::decompose_grep_pattern(pattern);
    let mut results = Vec::new();
    let mut gaps = Gaps::default();
    let mut phases = GrepExecutionPhaseTimings::default();
    let mut scope_has_files = false;
    for root in &scope.roots {
        let search_root = &root.search_root;
        if search_root.is_file() || !search_root.starts_with(session.root()) {
            let single = GrepScope {
                roots: vec![root.clone()],
                multi_root: false,
                per_root_max: scope.per_root_max,
            };
            let (result, root_phases) =
                crate::grep_executor::execute_profiled_with_filters_in_order(
                    ctx, pattern, &single, params, filters, order,
                );
            scope_has_files |= root_phases.indexed_scope_has_files.unwrap_or(true);
            results.push(result);
            continue;
        }
        let max_results = if scope.roots.len() > 1 {
            scope.per_root_max
        } else {
            params.max_results
        };
        let mut sources = Vec::new();
        for (child, text) in child_texts(ctx, &session, search_root) {
            let text = match text {
                Ok(text) => text,
                Err(reason) => {
                    gaps.child(&child, reason);
                    continue;
                }
            };
            if text.has_files(search_root) {
                sources.push((child, text));
            }
        }
        // Children are independent indexes, so they are searched in parallel:
        // the parent's latency then tracks its largest child, not the sum.
        // Their path-ordered scans run at once, so they share one row budget.
        let row_budget = crate::search_index::PATH_ORDER_BATCH_ROW_BUDGET / sources.len().max(1);
        use rayon::prelude::*;
        let answers = sources
            .par_iter()
            .map(|(child, text)| {
                let has_files = text.has_files_matching(search_root, filters);
                let snapshot = match text.candidates(pattern) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        // A child view that cannot be read fails the call
                        // with its cause, as a single checkout's does.
                        let error = session.note_child_failure(child, "parent_trigram", &error);
                        return (
                            has_files,
                            crate::grep_executor::failed_index_grep(std::io::Error::other(
                                crate::context::view_load_failure_message(error),
                            )),
                            crate::search_index::GrepQueryPhaseTimings::default(),
                        );
                    }
                };
                let (result, timings) = match snapshot
                    .search_grep_profiled_with_filters_and_query_in_order_with_row_budget(
                        pattern,
                        &query,
                        filters,
                        search_root,
                        max_results,
                        params.path_exclusion,
                        order,
                        row_budget,
                    ) {
                    Ok(result) => result,
                    Err(error) => (
                        crate::grep_executor::failed_index_grep(error),
                        crate::search_index::GrepQueryPhaseTimings::default(),
                    ),
                };
                (has_files, result, timings)
            })
            .collect::<Vec<_>>();
        for (has_files, result, timings) in answers {
            scope_has_files |= has_files;
            phases.query.trigram_lookup += timings.trigram_lookup;
            phases.query.pread_verify += timings.pread_verify;
            phases.query.candidate_count += timings.candidate_count;
            phases.query.bytes_verified += timings.bytes_verified;
            results.push(result);
        }
        gaps.scope(&session, search_root);
    }
    phases.indexed_scope_has_files = Some(scope_has_files || !gaps.is_empty());
    let result = if results.is_empty() {
        GrepResult {
            index_sources_unavailable: false,
            index_read_gaps: Vec::new(),
            matches: Vec::new(),
            total_matches: 0,
            files_searched: 0,
            files_with_matches: 0,
            index_status: IndexStatus::Ready,
            truncated: false,
            fully_degraded: false,
            engine_capped: false,
            walk_truncated: false,
            scan_deadline_reached: false,
            files_read_directly: 0,
            walk_bound: None,
            skipped_foreign_mounts: 0,
            missing_on_disk: 0,
        }
    } else {
        crate::grep_executor::merge_grep_results(results, &project_root, params.max_results)
    };
    Some((result, phases, gaps.into_values()))
}

/// The parent session's glob answer for `search_roots`.
pub(crate) struct GlobFanOut {
    pub files: Vec<PathBuf>,
    pub scope_has_files: bool,
    pub entries_visited: usize,
    pub gaps: Vec<Value>,
}

/// The parent session's glob: each root is answered from the warm trigram
/// snapshots of the children it overlaps. `None` when `ctx` is not a parent
/// session.
pub(crate) fn glob_fan_out(
    ctx: &AppContext,
    search_roots: &[PathBuf],
    pattern: &str,
) -> Option<GlobFanOut> {
    let session = super::session_for(ctx)?;
    let mut answer = GlobFanOut {
        files: Vec::new(),
        scope_has_files: false,
        entries_visited: 0,
        gaps: Vec::new(),
    };
    let mut gaps = Gaps::default();
    for search_root in search_roots {
        let search_root =
            std::fs::canonicalize(search_root).unwrap_or_else(|_| search_root.clone());
        if !search_root.starts_with(session.root()) {
            gaps.push(
                "outside_child_repositories",
                search_root.display().to_string(),
                "outside the parent folder; a parent folder indexes only its repositories".into(),
            );
            continue;
        }
        for (child, text) in child_texts(ctx, &session, &search_root) {
            let text = match text {
                Ok(text) => text,
                Err(reason) => {
                    gaps.child(&child, reason);
                    continue;
                }
            };
            let (files, scope_has_files, visited) =
                text.listing().glob_profiled(pattern, &search_root, false);
            answer.scope_has_files |= scope_has_files;
            answer.entries_visited += visited;
            answer.files.extend(files);
        }
        gaps.scope(&session, &search_root);
    }
    answer.files.sort();
    answer.files.dedup();
    answer.scope_has_files |= !gaps.is_empty();
    answer.gaps = gaps.into_values();
    Some(answer)
}

// ---------------------------------------------------------------------------
// Routed commands
// ---------------------------------------------------------------------------

/// Answers `req` from the children when `ctx` is a parent folder session and
/// the command reads an index. `None` lets the ordinary handler run.
pub fn route(req: &RawRequest, ctx: &AppContext) -> Option<Response> {
    if !ROUTED_COMMANDS.contains(&req.command.as_str()) {
        return None;
    }
    let session = super::session_for(ctx)?;
    Some(match req.command.as_str() {
        "semantic_search" => super::engine::search(req, ctx, &session),
        "inspect" => inspect(req, ctx, &session),
        operation => callgraph(req, ctx, &session, operation),
    })
}

fn string_param<'a>(req: &'a RawRequest, name: &str) -> Option<&'a str> {
    req.params.get(name).and_then(Value::as_str)
}

fn missing(req: &RawRequest, operation: &str, name: &str) -> Response {
    Response::error(
        &req.id,
        "invalid_request",
        format!("{operation}: missing required param '{name}'"),
    )
}

fn depth_param(req: &RawRequest, default: u64, max: u64) -> usize {
    req.params
        .get("depth")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .min(max) as usize
}

fn include_tests(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Applies the session's read-path authorization before resolving the path
/// to an absolute path within the parent folder.
fn resolve_path(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    raw: &str,
) -> Result<PathBuf, Response> {
    let validated = ctx.validate_read_path(&req.id, req.session(), Path::new(raw))?;
    let absolute = if validated.is_relative() {
        session.root().join(validated)
    } else {
        validated
    };
    Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

/// A response naming why the parent could not answer from one child.
fn gap_response(req: &RawRequest, operation: &str, gaps: Gaps) -> Response {
    let gaps = gaps.into_values();
    let reasons = gaps
        .iter()
        .map(|gap| {
            format!(
                "{}: {}",
                gap["path"].as_str().unwrap_or("."),
                gap["reason"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Response::error_with_data(
        &req.id,
        "parent_child_unavailable",
        format!("{operation}: not answered from this parent folder: {reasons}"),
        json!({"complete": false, "gaps": gaps}),
    )
}

/// Rewrites the call graph's checkout-relative paths to parent-relative ones.
fn prefix_paths(value: &mut Value, prefix: &Path) {
    const PATH_KEYS: &[&str] = &["file", "caller_file", "target_file", "to_file"];
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                match value {
                    Value::String(text) if PATH_KEYS.contains(&key.as_str()) => {
                        if !Path::new(text.as_str()).is_absolute() {
                            *text = prefix
                                .join(text.as_str())
                                .to_string_lossy()
                                .replace('\\', "/");
                        }
                    }
                    other => prefix_paths(other, prefix),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| prefix_paths(item, prefix)),
        _ => {}
    }
}

fn callgraph(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    operation: &str,
) -> Response {
    let Some(file) = string_param(req, "file") else {
        return missing(req, operation, "file");
    };
    let Some(symbol) = string_param(req, "symbol") else {
        return missing(req, operation, "symbol");
    };
    let file = match resolve_path(req, ctx, session, file) {
        Ok(file) => file,
        Err(response) => return response,
    };
    let mut gaps = Gaps::default();
    let Some(child) = session.child_for(&file) else {
        if !discovered(session, &mut gaps) {
            return gap_response(req, operation, gaps);
        }
        gaps.push(
            "outside_child_repositories",
            display_relative(session.root(), &file),
            "outside every child repository; a parent folder has no call graph of its own".into(),
        );
        return gap_response(req, operation, gaps);
    };
    if !session.planes.callgraph {
        gaps.child(&child, "call graph: the call graph is off".into());
        return gap_response(req, operation, gaps);
    }
    data_layer_callgraph(req, ctx, session, operation, &child, &file, symbol)
}

/// [`callgraph`] answered from the child's own checkout in the index store.
/// The parent keeps that checkout current itself and waits for its
/// recorded edits as a single checkout's query does, so the answer is
/// current.
fn data_layer_callgraph(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    operation: &str,
    child: &Child,
    file: &Path,
    symbol: &str,
) -> Response {
    let plane = crate::data_layer::parent::callgraph_plane(ctx, session.root(), &child.root);
    let Plane::Ready(view) = plane else {
        let mut gaps = Gaps::default();
        if let Some(reason) = plane.gap_reason("call graph") {
            gaps.child(child, reason);
        }
        return gap_response(req, operation, gaps);
    };
    let reader = view.callgraph();
    let answer = run_operation(req, ctx, session, operation, &reader, child, file, symbol);
    // A file whose part of the call graph could not be read was seen as
    // absent, so the answer cannot be trusted.
    if let Some(response) =
        crate::data_layer::tools::callgraph::failed_reads(&req.id, operation, Some(&view))
    {
        return response;
    }
    match answer {
        Ok(mut value) => {
            prefix_paths(&mut value, &child.relative);
            Response::success(&req.id, value)
        }
        Err(response) => response,
    }
}

/// Runs call-graph `operation` for `symbol` in `file` over `store`, the
/// call graph of `child`: the reply's value, with paths relative to the
/// child, or the response to send instead.
#[allow(clippy::too_many_arguments)]
fn run_operation(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    operation: &str,
    store: &impl crate::callgraph_store::CallGraphRead,
    child: &Child,
    file: &Path,
    symbol: &str,
) -> Result<Value, Response> {
    let tests = include_tests(req);
    let value = match operation {
        "callers" => adapter::callers_result(store, file, symbol, depth_param(req, 1, 100), tests)
            .map(|result| adapter::serialized_value(&req.id, operation, &result)),
        "call_tree" => {
            adapter::call_tree_result(store, file, symbol, depth_param(req, 5, 100), tests)
                .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        "impact" => adapter::impact_result(store, file, symbol, depth_param(req, 5, 100), tests)
            .map(|result| adapter::serialized_value(&req.id, operation, &result)),
        "trace_to" => {
            adapter::trace_to_result(store, file, symbol, depth_param(req, 10, 100), tests)
                .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        "trace_to_symbol" => {
            let Some(to_symbol) = string_param(req, "toSymbol") else {
                return Err(missing(req, operation, "toSymbol"));
            };
            let to_file = match string_param(req, "toFile") {
                Some(raw) => Some(resolve_path(req, ctx, session, raw)?),
                None => None,
            };
            if let Some(to_file) = to_file.as_deref() {
                if !to_file.starts_with(&child.root) {
                    let mut gaps = Gaps::default();
                    gaps.push(
                        "parent_cross_repository",
                        display_relative(session.root(), to_file),
                        format!(
                            "in a different repository than {}; each repository's call graph covers only its own files",
                            child.display()
                        ),
                    );
                    return Err(gap_response(req, operation, gaps));
                }
            }
            adapter::trace_to_symbol_result(
                store,
                file,
                symbol,
                to_symbol,
                to_file.as_deref(),
                depth_param(req, 10, 16),
                tests,
            )
            .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        "trace_data" => {
            let Some(expression) = string_param(req, "expression") else {
                return Err(missing(req, operation, "expression"));
            };
            adapter::trace_data_result(
                store,
                file,
                symbol,
                expression,
                depth_param(req, 5, 100),
                ctx.symbol_cache(),
            )
            .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        _ => {
            return Err(Response::error(
                &req.id,
                "unknown_command",
                operation.to_string(),
            ))
        }
    };
    match value {
        Ok(value) => value,
        Err(error) => Err(adapter::store_error_response(&req.id, operation, error)),
    }
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn inspect(req: &RawRequest, ctx: &AppContext, session: &ParentSession) -> Response {
    super::inspect::answer(req, ctx, session)
}

#[cfg(test)]
mod disk_reserve_tests {}
