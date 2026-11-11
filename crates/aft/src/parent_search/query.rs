//! Query fan-out for parent folder sessions: each request reads the warm
//! child planes the worker installed, merges their answers under the parent
//! root, and names every child (or area outside the children) it could not
//! answer for.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::context::AppContext;
use crate::grep_executor::{GrepExecutionPhaseTimings, GrepParams, GrepScope};
use crate::pattern_compile::CompiledPattern;
use crate::search_index::{GrepResult, IndexStatus, PathFilters, SearchIndexSnapshot};

use super::{display_relative, Child, ParentSession, Plane};

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
    attach_gaps_in_order(body, gaps, false);
}

/// Search gaps precede the counts: an unsearched child is not a negative result.
pub(crate) fn attach_search_gaps(body: &mut Value, gaps: Vec<Value>) {
    attach_gaps_in_order(body, gaps, true);
}

fn attach_gaps_in_order(body: &mut Value, gaps: Vec<Value>, first: bool) {
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
            let note = format!("(Parent folder: not covered by this answer:\n{lines})");
            if first {
                *text = format!("{note}\n\n{text}");
            } else {
                text.push_str(&format!("\n\n{note}"));
            }
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
    Data(crate::data_layer::text_scope::ViewScope),
    Loading,
}

impl ChildText {
    /// Whether the child holds any file under `search_root`.
    fn has_files(&self, search_root: &Path) -> bool {
        let _ = search_root;
        match self {
            Self::Data(scope) => scope.has_files(),
            Self::Loading => true,
        }
    }

    /// Whether the child holds any file under `search_root` that passes
    /// `filters`, which match paths relative to the child's root.
    fn has_files_matching(&self, search_root: &Path, filters: &PathFilters) -> bool {
        let _ = search_root;
        match self {
            Self::Data(scope) => scope.has_files_matching(filters),
            Self::Loading => unreachable!("loading children use the bounded walk"),
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
            Self::Loading => unreachable!("loading children use the bounded walk"),
        }
    }

    /// A snapshot listing the child's files, for glob.
    fn listing(&self) -> Cow<'_, SearchIndexSnapshot> {
        match self {
            Self::Data(scope) => Cow::Owned(scope.listing()),
            Self::Loading => unreachable!("loading children use the bounded walk"),
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
                        crate::data_layer::text_scope::ViewScope::of((*view).clone(), scope)?,
                    ))
                }
                Plane::Loading => Ok(ChildText::Loading),
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
/// warm trigram snapshots of the children it overlaps, walking children whose
/// indexes are still building. Explicit files and
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
            if matches!(text, ChildText::Loading) {
                let search_root = if search_root.starts_with(&child.root) {
                    search_root
                } else {
                    &child.root
                };
                let mut counts = crate::grep_executor::ScopeFileCounts::default();
                let result = crate::grep_executor::fallback_grep_counted(
                    &project_root,
                    search_root,
                    &child.root,
                    pattern,
                    filters,
                    max_results,
                    IndexStatus::Building,
                    params.path_exclusion,
                    &mut counts,
                    false,
                    order,
                );
                if let Some(reason) = result.index_read_gaps.first() {
                    gaps.child(&child, reason.clone());
                } else if let Some(reason) = crate::commands::grep::walk_coverage_note(&result) {
                    gaps.child(&child, reason);
                } else if result.skipped_foreign_mounts > 0 {
                    gaps.child(&child, format!(
                        "(Fallback directory walk skipped {} foreign filesystem mount(s); results may be incomplete.)",
                        result.skipped_foreign_mounts
                    ));
                }
                scope_has_files |= counts.eligible > 0;
                phases
                    .walk_counts
                    .get_or_insert_with(Default::default)
                    .add(counts);
                results.push(result);
                continue;
            }
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
                                crate::data_layer::status::view_load_failure_message(error),
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
    pub walked: bool,
    pub walk_truncated: bool,
    pub walk_time_limit_reached: bool,
    pub skipped_foreign_mounts: usize,
    pub gaps: Vec<Value>,
}

/// The parent session's glob: each root is answered from the warm trigram
/// snapshots of the children it overlaps, walking children whose indexes are
/// still building. `None` when `ctx` is not a parent
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
        walked: false,
        walk_truncated: false,
        walk_time_limit_reached: false,
        skipped_foreign_mounts: 0,
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
            if matches!(text, ChildText::Loading) {
                let child_root = if search_root.starts_with(&child.root) {
                    &search_root
                } else {
                    &child.root
                };
                let filters = crate::search_index::build_path_filters(&[pattern.to_string()], &[])
                    .unwrap_or_default();
                let outcome = crate::grep_executor::bounded_fallback_walk_files_for_target(
                    &search_root,
                    child_root,
                    &filters,
                    false,
                );
                if let Some(reason) = outcome.error {
                    gaps.child(&child, reason);
                } else if outcome.walk_truncated {
                    gaps.child(&child, "(Fallback directory walk stopped early: file-count or time budget reached; results may be incomplete.)".into());
                } else if outcome.skipped_foreign_mounts > 0 {
                    gaps.child(&child, format!(
                        "(Fallback directory walk skipped {} foreign filesystem mount(s); results may be incomplete.)",
                        outcome.skipped_foreign_mounts
                    ));
                }
                answer.walked = true;
                answer.walk_truncated |= outcome.walk_truncated;
                answer.walk_time_limit_reached |= outcome.walk_time_limit_reached;
                answer.skipped_foreign_mounts += outcome.skipped_foreign_mounts;
                answer.scope_has_files |= !outcome.files.is_empty();
                answer.entries_visited += outcome.entries_visited;
                answer.files.extend(outcome.files);
                continue;
            }
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
