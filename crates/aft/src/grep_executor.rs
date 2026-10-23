use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use rayon::prelude::*;

use crate::commands::multi_path::{
    canonical_key, dedupe_nested_paths, resolve_path_or_multi, SearchPathResolution,
};
use crate::context::AppContext;
use crate::pattern_compile::{CompiledPattern, LiteralSearch};
use crate::protocol::Response;
use crate::search_index::{
    build_path_filters, decompose_grep_pattern, resolve_search_scope,
    sort_grep_matches_by_mtime_desc, sort_walked_paths_by_mtime_desc, try_read_with_budget,
    GrepMatch, GrepPathExclusion, GrepQueryPhaseTimings, GrepResult, GrepScanOrder, IndexStatus,
    PathFilters, RegexQuery, WalkBound, INTERACTIVE_ARTIFACT_READ_BUDGET,
};

/// Maximum files enumerated during grep/glob index-unavailable fallback walks.
pub(crate) const MAX_FALLBACK_WALK_FILES: usize = 50_000;

#[cfg(test)]
thread_local! {
    static WALK_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_FALLBACK_WALK_FILES) };
    static WALK_BUDGET: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::from_secs(10)) };
    static WALK_ENTRY_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    static WALK_SORT_METADATA_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

fn fallback_walk_limit() -> usize {
    #[cfg(test)]
    {
        WALK_LIMIT.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        MAX_FALLBACK_WALK_FILES
    }
}

#[cfg(test)]
pub(crate) fn with_walk_limit<R>(limit: usize, run: impl FnOnce() -> R) -> R {
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            WALK_LIMIT.with(|limit| limit.set(self.0));
        }
    }
    let _restore = Restore(WALK_LIMIT.with(|current| current.replace(limit)));
    run()
}
/// Wall-clock budget for grep/glob index-unavailable fallback walks on the dispatch thread.
pub(crate) const FALLBACK_WALK_BUDGET: Duration = Duration::from_secs(10);
const FALLBACK_WALK_RETURN_MARGIN: Duration = Duration::from_millis(100);

fn fallback_walk_budget() -> Duration {
    #[cfg(test)]
    {
        return WALK_BUDGET.with(std::cell::Cell::get);
    }
    #[cfg(not(test))]
    {
        FALLBACK_WALK_BUDGET
    }
}

fn fallback_walk_entry_delay() -> Duration {
    #[cfg(test)]
    {
        return WALK_ENTRY_DELAY.with(std::cell::Cell::get);
    }
    #[cfg(not(test))]
    {
        Duration::ZERO
    }
}

fn fallback_walk_sort_metadata_delay() -> Duration {
    #[cfg(test)]
    {
        return WALK_SORT_METADATA_DELAY.with(std::cell::Cell::get);
    }
    #[cfg(not(test))]
    {
        Duration::ZERO
    }
}

#[cfg(test)]
pub(crate) fn with_fallback_walk_budget_for_test<R>(
    budget: Duration,
    run: impl FnOnce() -> R,
) -> R {
    struct Restore(Duration);
    impl Drop for Restore {
        fn drop(&mut self) {
            WALK_BUDGET.with(|current| current.set(self.0));
        }
    }
    let _restore = Restore(WALK_BUDGET.with(|current| current.replace(budget)));
    run()
}

#[cfg(test)]
pub(crate) fn with_fallback_walk_entry_delay_for_test<R>(
    delay: Duration,
    run: impl FnOnce() -> R,
) -> R {
    struct Restore(Duration);
    impl Drop for Restore {
        fn drop(&mut self) {
            WALK_ENTRY_DELAY.with(|current| current.set(self.0));
        }
    }
    let _restore = Restore(WALK_ENTRY_DELAY.with(|current| current.replace(delay)));
    run()
}

#[cfg(test)]
pub(crate) fn with_fallback_walk_sort_metadata_delay_for_test<R>(
    delay: Duration,
    run: impl FnOnce() -> R,
) -> R {
    struct Restore(Duration);
    impl Drop for Restore {
        fn drop(&mut self) {
            WALK_SORT_METADATA_DELAY.with(|current| current.set(self.0));
        }
    }
    let _restore = Restore(WALK_SORT_METADATA_DELAY.with(|current| current.replace(delay)));
    run()
}

#[derive(Clone, Debug)]
pub struct FallbackWalkOutcome {
    pub files: Vec<PathBuf>,
    pub walk_truncated: bool,
    /// True only when the iterator stopped because its walk deadline expired.
    pub walk_time_limit_reached: bool,
    /// Foreign filesystem mounts skipped before a recursive fallback could open them.
    pub skipped_foreign_mounts: usize,
    pub entries_visited: usize,
}

/// Counts are updated before include/exclude filtering. Empty-scope probes count
/// rejected directories once, without enumerating their contents.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScopeFileCounts {
    pub examined: usize,
    pub ignored: usize,
    pub filtered: usize,
    pub eligible: usize,
    pub entries_visited: usize,
}

impl ScopeFileCounts {
    pub(crate) fn add(&mut self, other: Self) {
        self.examined += other.examined;
        self.ignored += other.ignored;
        self.filtered += other.filtered;
        self.eligible += other.eligible;
        self.entries_visited += other.entries_visited;
    }

    pub(crate) fn empty_note(&self, include_only: bool) -> String {
        self.empty_note_with_hint(include_only, "pass the file path directly to search them")
    }

    pub(crate) fn empty_glob_note(&self) -> String {
        self.empty_note_with_hint(true, "name the ignored folder in the pattern to search it")
    }

    fn empty_note_with_hint(&self, include_only: bool, hint: &str) -> String {
        let mut reasons = Vec::new();
        if self.ignored > 0 {
            reasons.push(format!(
                "{} ignored items (directories counted once) excluded by ignore rules or default skips (.gitignore / info/exclude / .ignore / .aftignore); {hint}",
                self.ignored
            ));
        }
        if self.filtered > 0 {
            reasons.push(if include_only {
                format!(
                    "the include pattern matched none of {} files",
                    self.filtered
                )
            } else {
                format!("include/exclude patterns excluded {} files", self.filtered)
            });
        }
        if reasons.is_empty() {
            if self.examined == 0 {
                return crate::commands::grep::NO_FILES_IN_SCOPE_NOTE.to_string();
            }
            reasons.push(format!(
                "{} files were examined but none were searched",
                self.examined
            ));
        }
        format!(
            "(0 files searched: {}; nothing was searched.)",
            reasons.join("; ")
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FallbackWalkProgress {
    /// The limit that stopped the walk early; `None` when it reached every file.
    bound: Option<WalkBound>,
    skipped_foreign_mounts: usize,
    counts: ScopeFileCounts,
}

#[derive(Clone, Debug)]
pub struct GrepParams {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_results: usize,
    pub path_exclusion: Option<GrepPathExclusion>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GrepExecutionPhaseTimings {
    pub snapshot_acquire: Duration,
    /// Regex decomposition is paid once per grep request, not once per root.
    pub query_decomposition: Duration,
    pub query: GrepQueryPhaseTimings,
    pub indexed_scope_has_files: Option<bool>,
    pub walk_counts: Option<ScopeFileCounts>,
}

impl GrepExecutionPhaseTimings {
    fn add(&mut self, other: Self) {
        self.snapshot_acquire += other.snapshot_acquire;
        self.query.trigram_lookup += other.query.trigram_lookup;
        self.query.pread_verify += other.query.pread_verify;
        self.query.post_filter += other.query.post_filter;
        self.query.candidate_count += other.query.candidate_count;
        self.query.bytes_verified += other.query.bytes_verified;
        self.walk_counts = match (self.walk_counts, other.walk_counts) {
            (Some(mut left), Some(right)) => {
                left.add(right);
                Some(left)
            }
            _ => None,
        };
        self.indexed_scope_has_files =
            match (self.indexed_scope_has_files, other.indexed_scope_has_files) {
                (Some(left), Some(right)) => Some(left || right),
                _ => None,
            };
    }
}

#[derive(Clone, Debug)]
pub struct GrepScope {
    pub roots: Vec<ResolvedRoot>,
    pub multi_root: bool,
    pub per_root_max: usize,
}

#[derive(Clone, Debug)]
pub struct ResolvedRoot {
    pub search_root: PathBuf,
    pub filter_root: PathBuf,
    pub use_index: bool,
    pub is_external: bool,
    pub ignored_target: bool,
    authorized_task_artifact: bool,
}

pub fn project_root(ctx: &AppContext) -> PathBuf {
    let project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    crate::bounded_io::canonicalize(&project_root).unwrap_or(project_root)
}

/// The test-only finishing order for a path-ordered fallback batch, or
/// `None` (outside tests always) to search the batch in parallel.
fn path_batch_order_for_test(search_root: &Path) -> Option<bool> {
    #[cfg(test)]
    {
        scan_order_for_test::get(search_root)
    }
    #[cfg(not(test))]
    {
        let _ = search_root;
        None
    }
}

/// Test-only override of the order in which the files of a path-ordered
/// fallback batch finish: when set for a search root, the batch's files are
/// searched one at a time, forward or reversed, instead of in parallel.
#[cfg(test)]
pub(crate) mod scan_order_for_test {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    static ORDERS: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();

    pub(crate) fn set(root: &Path, reversed: bool) {
        if let Ok(mut orders) = ORDERS.get_or_init(Default::default).lock() {
            orders.insert(root.to_path_buf(), reversed);
        }
    }

    pub(crate) fn get(root: &Path) -> Option<bool> {
        ORDERS
            .get_or_init(Default::default)
            .lock()
            .ok()
            .and_then(|orders| orders.get(root).copied())
    }
}

pub(crate) fn resolve_grep_scope(
    ctx: &AppContext,
    paths: Option<&serde_json::Value>,
    max_results: usize,
    req_id: &str,
    session_id: &str,
    search_tool: Option<crate::context::WorkerSearchTool>,
) -> Result<GrepScope, Response> {
    let project_root = project_root(ctx);
    let search_roots = resolve_roots(ctx, paths, &project_root, req_id, session_id, search_tool)?;

    if let Some(missing_root) = search_roots
        .iter()
        .find(|root| crate::bounded_io::metadata(root).is_err())
    {
        return Err(Response::error(
            req_id,
            "path_not_found",
            format!(
                "grep: search path does not exist: {}",
                missing_root.display()
            ),
        ));
    }

    // In a confined call, refuse a search root outside the confined root or
    // denied by the read policy before any walk starts there. A confined call
    // also skips the shortcut that answers from the session's own bash task
    // output files, because that shortcut reads them without walking.
    for search_root in &search_roots {
        crate::context::confinement::check_walk_root(req_id, search_root)?;
    }
    let confined = crate::context::confinement::active();
    let roots = search_roots
        .into_iter()
        .flat_map(|search_root| {
            let outputs = paths
                .filter(|paths| !confined && !paths.is_null())
                .and_then(|_| {
                    ctx.bash_background()
                        .explicit_task_artifacts(session_id, &search_root)
                });
            match outputs {
                Some(outputs) => {
                    let filter_root = if search_root.is_dir() {
                        search_root
                    } else {
                        search_root.parent().unwrap_or(&search_root).to_path_buf()
                    };
                    outputs
                        .into_iter()
                        .map(|path| (path, Some(filter_root.clone())))
                        .collect::<Vec<_>>()
                }
                None => vec![(search_root, None)],
            }
        })
        .map(|(search_root, task_filter_root)| {
            let scope = resolve_search_scope(&project_root, Some(&search_root.to_string_lossy()));
            let is_external = !scope.use_index;
            let ignored_target = !crate::context::confinement::active()
                && paths.is_some_and(|paths| !paths.is_null())
                && !is_same_directory(&scope.root, &project_root)
                && target_is_ignored(&scope.root);
            let authorized_task_artifact = task_filter_root.is_some();
            let filter_root = if let Some(filter_root) = task_filter_root {
                filter_root
            } else if ignored_target {
                // An ignored target is outside the index's project-relative
                // catalog. Like glob's fallback, interpret its include/exclude
                // patterns relative to the directory the caller explicitly named.
                scope.root.clone()
            } else {
                compute_filter_root(&project_root, &scope.root, scope.use_index, is_external)
            };
            ResolvedRoot {
                search_root: scope.root,
                filter_root,
                use_index: scope.use_index,
                is_external,
                ignored_target,
                authorized_task_artifact,
            }
        })
        .collect::<Vec<_>>();

    let multi_root = roots.len() > 1;
    let per_root_max = if multi_root {
        max_results.saturating_mul(2).max(max_results)
    } else {
        max_results
    };

    Ok(GrepScope {
        roots,
        multi_root,
        per_root_max,
    })
}

/// Runs one grep below a single starting path the way a request does once its
/// scope is resolved, without that resolution or its path checks, so tests
/// can show what the search itself does with a starting path.
#[cfg(all(test, unix))]
pub(crate) fn grep_root_for_test(
    ctx: &AppContext,
    search_root: &Path,
    pattern: &str,
) -> GrepResult {
    let pattern = match crate::pattern_compile::compile(
        pattern,
        crate::pattern_compile::CompileOpts::default(),
    ) {
        crate::pattern_compile::CompileResult::Ok(compiled) => compiled,
        other => panic!("compile regex {pattern:?}: {other:?}"),
    };
    let scope = GrepScope {
        roots: vec![ResolvedRoot {
            search_root: search_root.to_path_buf(),
            filter_root: search_root.to_path_buf(),
            use_index: false,
            is_external: true,
            ignored_target: false,
            authorized_task_artifact: false,
        }],
        multi_root: false,
        per_root_max: 16,
    };
    let params = GrepParams {
        include: Vec::new(),
        exclude: Vec::new(),
        max_results: 16,
        path_exclusion: None,
    };
    execute_profiled_with_filters(ctx, &pattern, &scope, &params, &PathFilters::default()).0
}

pub fn compute_filter_root(
    project_root: &Path,
    search_root: &Path,
    use_index: bool,
    is_external: bool,
) -> PathBuf {
    if is_external && !use_index {
        search_root.to_path_buf()
    } else {
        project_root.to_path_buf()
    }
}

pub(crate) fn scope_has_files(scope: &GrepScope, filters: &PathFilters) -> Option<bool> {
    let mut unknown = false;
    for root in &scope.roots {
        // A starting path a confined walk may not use has nothing in scope.
        if !crate::context::confinement::admits_walk_root(&root.search_root) {
            continue;
        }
        // An explicitly-named existing file is always in scope (it's searched
        // directly even if gitignored / .aftignored), so don't report it as
        // "no files matched scope".
        if crate::bounded_io::metadata(&root.search_root).is_ok_and(|m| m.is_file()) {
            return Some(true);
        }
        match bounded_scope_has_files(
            &root.filter_root,
            &root.search_root,
            filters,
            root.ignored_target,
        ) {
            Some(true) => return Some(true),
            None => unknown = true,
            Some(false) => {}
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

/// Used for an empty indexed answer, whose catalog cannot count ignored files.
pub(crate) fn scope_file_counts(
    scope: &GrepScope,
    filters: &PathFilters,
) -> Option<ScopeFileCounts> {
    let mut counts = ScopeFileCounts::default();
    for root in &scope.roots {
        counts.add(diagnose_scope_counts(
            &root.filter_root,
            &root.search_root,
            filters,
            root.ignored_target,
        )?);
    }
    Some(counts)
}

/// Stop at the first eligible file; an exhausted probe cannot prove an empty scope.
fn bounded_scope_has_files(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    ignored_target: bool,
) -> Option<bool> {
    let filters = filters.clone();
    let filter_root = filter_root.to_path_buf();
    crate::bounded_io::run(search_root, None, move |search_root| {
        Ok(scope_has_files_unbounded(
            &filter_root,
            &search_root,
            &filters,
            ignored_target,
        ))
    })
    .ok()
    .flatten()
}

fn scope_has_files_unbounded(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    ignored_target: bool,
) -> Option<bool> {
    let started = Instant::now();
    let skipped = Arc::new(AtomicUsize::new(0));
    let Some(builder) = fallback_target_walk_builder(
        search_root,
        Arc::clone(&skipped),
        ignored_target,
        false,
        Some(filter_root),
    ) else {
        return Some(false);
    };
    let walker = builder.build();
    for (visited, entry) in walker.enumerate() {
        if visited >= MAX_FALLBACK_WALK_FILES.saturating_mul(8)
            || started.elapsed() >= FALLBACK_WALK_BUDGET
            || crate::executor::current_job_cancelled()
        {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && filters.matches(filter_root, entry.path())
        {
            return Some(true);
        }
    }
    (skipped.load(Ordering::Relaxed) == 0).then_some(false)
}

pub fn execute(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    scope: &GrepScope,
    params: &GrepParams,
) -> GrepResult {
    execute_profiled(ctx, pattern, scope, params).0
}

pub(crate) fn execute_profiled(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    scope: &GrepScope,
    params: &GrepParams,
) -> (GrepResult, GrepExecutionPhaseTimings) {
    let _io_scope = crate::bounded_io::enter_if_absent(FALLBACK_WALK_BUDGET);
    let filters = build_path_filters(&params.include, &params.exclude).unwrap_or_default();
    execute_profiled_with_filters(ctx, pattern, scope, params, &filters)
}

pub(crate) fn execute_profiled_with_filters(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    scope: &GrepScope,
    params: &GrepParams,
    filters: &PathFilters,
) -> (GrepResult, GrepExecutionPhaseTimings) {
    execute_profiled_with_filters_in_order(
        ctx,
        pattern,
        scope,
        params,
        filters,
        GrepScanOrder::Arrival,
    )
}

/// [`execute_profiled_with_filters`] with a choice of which lines are kept
/// once the search passes `params.max_results`: the first found, or the
/// first by path and line.
pub(crate) fn execute_profiled_with_filters_in_order(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    scope: &GrepScope,
    params: &GrepParams,
    filters: &PathFilters,
    order: GrepScanOrder,
) -> (GrepResult, GrepExecutionPhaseTimings) {
    let project_root = project_root(ctx);
    let query_started = Instant::now();
    let query = decompose_grep_pattern(pattern);
    let query_decomposition = query_started.elapsed();
    if scope.roots.len() == 1 {
        let (result, mut phases) = execute_root_profiled(
            ctx,
            pattern,
            &query,
            &scope.roots[0],
            params,
            filters,
            params.max_results,
            &project_root,
            order,
        );
        phases.query_decomposition = query_decomposition;
        return (result, phases);
    }

    let mut results = Vec::new();
    let mut phases: Option<GrepExecutionPhaseTimings> = None;
    for root in &scope.roots {
        let (result, root_phases) = execute_root_profiled(
            ctx,
            pattern,
            &query,
            root,
            params,
            filters,
            scope.per_root_max,
            &project_root,
            order,
        );
        results.push(result);
        if let Some(phases) = phases.as_mut() {
            phases.add(root_phases);
        } else {
            phases = Some(root_phases);
        }
    }
    let mut phases = phases.unwrap_or_default();
    phases.query_decomposition = query_decomposition;
    (
        merge_grep_results(results, &project_root, params.max_results),
        phases,
    )
}

fn resolve_roots(
    ctx: &AppContext,
    paths: Option<&serde_json::Value>,
    project_root: &Path,
    req_id: &str,
    session_id: &str,
    search_tool: Option<crate::context::WorkerSearchTool>,
) -> Result<Vec<PathBuf>, Response> {
    let Some(paths) = paths else {
        return Ok(vec![resolve_search_scope(project_root, None).root]);
    };
    if paths.is_null() {
        return Ok(vec![resolve_search_scope(project_root, None).root]);
    }
    if let Some(path) = paths.as_str() {
        return match resolve_path_or_multi(
            path,
            project_root,
            |candidate| match search_tool {
                Some(search_tool) => {
                    ctx.validate_search_path(req_id, session_id, candidate, search_tool)
                }
                None => ctx.validate_read_path(req_id, session_id, candidate),
            },
            req_id,
        )? {
            SearchPathResolution::Single(root) => Ok(vec![root]),
            SearchPathResolution::Multi(roots) => Ok(roots),
        };
    }
    if let Some(items) = paths.as_array() {
        let mut roots = Vec::with_capacity(items.len());
        for item in items {
            let Some(path) = item.as_str() else {
                return Err(Response::error(
                    req_id,
                    "invalid_request",
                    "grep: path array entries must be strings",
                ));
            };
            let validated = match search_tool {
                Some(search_tool) => {
                    ctx.validate_search_path(req_id, session_id, Path::new(path), search_tool)?
                }
                None => ctx.validate_read_path(req_id, session_id, Path::new(path))?,
            };
            let raw = validated.to_string_lossy();
            roots.push(resolve_search_scope(project_root, Some(raw.as_ref())).root);
        }
        let roots = dedupe_nested_paths(roots);
        if roots.is_empty() {
            Ok(vec![resolve_search_scope(project_root, None).root])
        } else {
            Ok(roots)
        }
    } else {
        Err(Response::error(
            req_id,
            "invalid_request",
            "grep: path must be a string, array of strings, or null",
        ))
    }
}

fn execute_root_profiled(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    query: &RegexQuery,
    root: &ResolvedRoot,
    params: &GrepParams,
    filters: &PathFilters,
    max_results: usize,
    project_root: &Path,
    order: GrepScanOrder,
) -> (GrepResult, GrepExecutionPhaseTimings) {
    // A confined search never starts outside its fence, whether at a named
    // file (read directly below, not walked) or at a directory.
    if !crate::context::confinement::admits_walk_root(&root.search_root) {
        return (
            empty_grep_result(IndexStatus::Fallback, false),
            GrepExecutionPhaseTimings::default(),
        );
    }
    // Explicit single-file scope: search the named file directly, bypassing the
    // trigram index and the gitignore/.aftignore-aware walk. Matches ripgrep,
    // where naming a file explicitly searches it even when it is gitignored,
    // .aftignored, or not yet indexed. Binary + UTF-8 guards still apply.
    if crate::bounded_io::metadata(&root.search_root).is_ok_and(|m| m.is_file()) {
        if root.use_index && !crate::context::confinement::active() {
            crate::commands::configure::trigger_search_index_reload_if_evicted(ctx);
        }
        let index_status = if root.use_index && !crate::context::confinement::active() {
            current_index_status(ctx)
        } else {
            IndexStatus::Fallback
        };
        let result = if (root.authorized_task_artifact
            && !filters.matches(&root.filter_root, &root.search_root))
            || params
                .path_exclusion
                .is_some_and(|exclude| exclude(&root.search_root, project_root))
        {
            empty_grep_result(index_status, false)
        } else {
            grep_explicit_file_with_output(
                &root.search_root,
                pattern,
                max_results,
                index_status,
                root.authorized_task_artifact,
            )
        };
        return (result, GrepExecutionPhaseTimings::default());
    }

    if crate::context::confinement::active() {
        let mut counts = ScopeFileCounts::default();
        let result = fallback_grep_counted(
            project_root,
            &root.search_root,
            &root.filter_root,
            pattern,
            filters,
            max_results,
            IndexStatus::Fallback,
            params.path_exclusion,
            &mut counts,
            root.ignored_target,
            order,
        );
        return (
            result,
            GrepExecutionPhaseTimings {
                walk_counts: Some(counts),
                ..Default::default()
            },
        );
    }

    let snapshot_started = Instant::now();
    let ignored_target = root.ignored_target;
    let mut snapshot_timed_out = false;
    let indexed_snapshot =
        match try_read_with_budget(ctx.search_index(), INTERACTIVE_ARTIFACT_READ_BUDGET) {
            Some(search_index) => match search_index.as_ref() {
                Some(index) if index.ready && root.use_index && !ignored_target => {
                    Some(index.snapshot())
                }
                _ => None,
            },
            None => {
                snapshot_timed_out = true;
                None
            }
        };
    let snapshot_acquire = snapshot_started.elapsed();
    let mut index_lacks_scope = ignored_target;
    if let Some(snapshot) = indexed_snapshot {
        let scope_started = Instant::now();
        let indexed_scope_has_files = snapshot.has_file_in_scope(&root.search_root);
        let scope_elapsed = scope_started.elapsed();
        // A project-wide search always answers from the index, even when it
        // holds no files (everything ignored, or an empty project): that is a
        // real, complete answer, not a missing scope.
        if indexed_scope_has_files || is_same_directory(&root.search_root, project_root) {
            let (result, mut query_timings) = snapshot
                .search_grep_profiled_with_filters_and_query_in_order(
                    pattern,
                    query,
                    filters,
                    &root.search_root,
                    max_results,
                    params.path_exclusion,
                    order,
                );
            query_timings.post_filter += scope_elapsed;
            return (
                result,
                GrepExecutionPhaseTimings {
                    snapshot_acquire,
                    query_decomposition: Duration::ZERO,
                    query: query_timings,
                    indexed_scope_has_files: Some(
                        snapshot.has_file_in_scope_with_filters(&root.search_root, filters),
                    ),
                    walk_counts: None,
                },
            );
        }
        // The index holds no file under this explicitly named subdirectory. That is the case when the
        // caller named a directory an ignore rule keeps out of the index (a
        // gitignored `node_modules/pkg`, for example) or a symlinked directory
        // the index walk never follows. Answering from the index would report
        // zero matches for a directory that may be full of files, so walk it
        // instead. This follows ripgrep: the walk never applies ignore rules to
        // the named root itself, only to entries nested inside it. The walk
        // keeps its usual file-count and time bounds.
        index_lacks_scope = true;
    }

    if root.use_index && !index_lacks_scope {
        crate::commands::configure::trigger_search_index_reload_if_evicted(ctx);
    }
    // A walk that replaces a ready index reports `Fallback`, so the response
    // discloses that these results came from the filesystem, not the index.
    let index_status = if root.use_index && !index_lacks_scope {
        if snapshot_timed_out {
            IndexStatus::Fallback
        } else {
            current_index_status(ctx)
        }
    } else {
        IndexStatus::Fallback
    };
    let mut counts = ScopeFileCounts::default();
    let result = fallback_grep_counted(
        project_root,
        &root.search_root,
        &root.filter_root,
        pattern,
        filters,
        max_results,
        index_status,
        params.path_exclusion,
        &mut counts,
        ignored_target,
        order,
    );
    (
        result,
        GrepExecutionPhaseTimings {
            snapshot_acquire,
            walk_counts: Some(counts),
            ..GrepExecutionPhaseTimings::default()
        },
    )
}

/// True when both paths name the same directory, comparing the given forms
/// first and then their canonical forms (symlinks, `\\?\` prefixes).
pub(crate) fn is_same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || crate::bounded_io::canonicalize(left)
            .ok()
            .zip(crate::bounded_io::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn empty_grep_result(index_status: IndexStatus, fully_degraded: bool) -> GrepResult {
    GrepResult {
        matches: Vec::new(),
        total_matches: 0,
        files_searched: 0,
        files_with_matches: 0,
        index_status,
        truncated: false,
        fully_degraded,
        engine_capped: false,
        walk_truncated: false,
        skipped_foreign_mounts: 0,
        missing_on_disk: 0,
        scan_deadline_reached: false,
        files_read_directly: 0,
        walk_bound: None,
    }
}

/// Grep a single explicitly-named file directly, bypassing the trigram index
/// and the gitignore/.aftignore-aware walk. Used when the caller's `path`
/// resolves to one existing file — ripgrep semantics: an explicitly-named file
/// is searched even when it is gitignored, `.aftignore`d, or not yet indexed.
/// Binary detection and UTF-8 guards still apply (via `read_searchable_text`
/// inside `fallback_search_file`).
#[cfg(test)]
fn grep_explicit_file(
    file: &Path,
    pattern: &CompiledPattern,
    max_results: usize,
    index_status: IndexStatus,
) -> GrepResult {
    grep_explicit_file_with_output(file, pattern, max_results, index_status, false)
}

fn grep_explicit_file_with_output(
    file: &Path,
    pattern: &CompiledPattern,
    max_results: usize,
    index_status: IndexStatus,
    authorized_task_artifact: bool,
) -> GrepResult {
    let _io_scope = crate::bounded_io::enter_if_absent(FALLBACK_WALK_BUDGET);
    let total_matches = AtomicUsize::new(0);
    let files_searched = AtomicUsize::new(0);
    let files_with_matches = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let engine_capped = AtomicBool::new(false);
    let stop_after = max_results.saturating_mul(2);
    let job_cancellation = crate::executor::current_job_cancellation();
    // A single named file gets the walk's time budget too: one huge file
    // with a slow pattern must not hold the request open indefinitely.
    let deadline = Instant::now() + FALLBACK_WALK_BUDGET;

    let matches = fallback_search_file(
        &file.to_path_buf(),
        pattern,
        max_results,
        stop_after,
        &total_matches,
        &files_searched,
        &files_with_matches,
        &truncated,
        &engine_capped,
        job_cancellation.as_ref(),
        Some(deadline),
        authorized_task_artifact,
    );
    let engine_capped = engine_capped.load(Ordering::Relaxed);
    let files_searched = files_searched.load(Ordering::Relaxed);

    GrepResult {
        total_matches: total_matches.load(Ordering::Relaxed),
        matches,
        files_searched,
        files_with_matches: files_with_matches.load(Ordering::Relaxed),
        index_status,
        truncated: truncated.load(Ordering::Relaxed),
        fully_degraded: false,
        engine_capped,
        walk_truncated: false,
        skipped_foreign_mounts: 0,
        missing_on_disk: 0,
        scan_deadline_reached: engine_capped && Instant::now() >= deadline,
        files_read_directly: files_searched,
        walk_bound: None,
    }
}

pub fn merge_grep_results(
    results: Vec<GrepResult>,
    project_root: &Path,
    max_results: usize,
) -> GrepResult {
    let mut matches = Vec::new();
    let mut total_matches = 0usize;
    let mut files_searched = 0usize;
    let mut files_with_matches = 0usize;
    let mut index_status = IndexStatus::Ready;
    let mut any_child_truncated = false;
    let mut fully_degraded = false;
    let mut engine_capped = false;
    let mut walk_truncated = false;
    let mut skipped_foreign_mounts = 0usize;
    let mut missing_on_disk = 0usize;
    let mut scan_deadline_reached = false;
    let mut files_read_directly = 0usize;
    let mut walk_bound = None;
    let mut seen_match_keys = HashSet::new();

    for result in results {
        total_matches += result.total_matches;
        files_searched += result.files_searched;
        files_with_matches += result.files_with_matches;
        index_status = weakest_index_status(index_status, result.index_status);
        any_child_truncated |= result.truncated;
        fully_degraded |= result.fully_degraded;
        engine_capped |= result.engine_capped;
        walk_truncated |= result.walk_truncated;
        skipped_foreign_mounts += result.skipped_foreign_mounts;
        missing_on_disk += result.missing_on_disk;
        scan_deadline_reached |= result.scan_deadline_reached;
        files_read_directly += result.files_read_directly;
        walk_bound = walk_bound.or(result.walk_bound);

        for grep_match in result.matches {
            let file_key = canonical_key(&grep_match.file);
            let match_key = (file_key, grep_match.line, grep_match.column);
            if seen_match_keys.insert(match_key) {
                matches.push(grep_match);
            }
        }
    }

    sort_grep_matches_by_mtime_desc(&mut matches, project_root);
    if matches.len() > max_results {
        matches.truncate(max_results);
    }

    GrepResult {
        matches,
        total_matches,
        files_searched,
        files_with_matches,
        index_status,
        truncated: any_child_truncated || total_matches > max_results,
        fully_degraded,
        engine_capped,
        walk_truncated,
        skipped_foreign_mounts,
        missing_on_disk,
        scan_deadline_reached,
        files_read_directly,
        walk_bound,
    }
}

/// The whole-project walker semantic search sweeps with. Confined calls never
/// run semantic search, so this walk does not check its starting path; grep,
/// glob and their scope probes use [`fallback_target_walk_builder`], which
/// does.
pub(crate) fn fallback_project_walk_builder(
    search_root: &Path,
    skipped_foreign_mounts: Arc<AtomicUsize>,
) -> WalkBuilder {
    target_walk_builder(
        search_root,
        skipped_foreign_mounts,
        false,
        false,
        Some(search_root),
    )
}

/// The grep/glob directory walker for `search_root`, or `None` when a
/// confined walk may not start there (outside the confined root, or denied
/// by the read policy). The walker never filters its own starting path, so
/// that path is checked here, before anything under it is opened; callers
/// treat `None` as a walk that found nothing.
fn fallback_target_walk_builder(
    search_root: &Path,
    skipped_foreign_mounts: Arc<AtomicUsize>,
    ignored_target: bool,
    ignore_ancestors: bool,
    filter_root: Option<&Path>,
) -> Option<WalkBuilder> {
    if !crate::context::confinement::admits_walk_root(search_root) {
        return None;
    }
    Some(target_walk_builder(
        search_root,
        skipped_foreign_mounts,
        ignored_target,
        ignore_ancestors,
        filter_root,
    ))
}

/// The grep/glob directory walker for `search_root`; a confined walk checks
/// every entry below the starting path, but not the starting path itself.
fn target_walk_builder(
    search_root: &Path,
    skipped_foreign_mounts: Arc<AtomicUsize>,
    ignored_target: bool,
    ignore_ancestors: bool,
    filter_root: Option<&Path>,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(search_root);
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(search_root).ok();
    // A confined walk loads none of the crate's ignore files (see
    // `apply_project_ignore_rules`); it applies the confined root's own
    // ignore files here and checks every entry against the read policy.
    let confined = crate::context::confinement::ConfinedIgnoreRules::current(
        crate::context::confinement::SEARCH_IGNORE_FILES,
    );
    let ignore_root = ignore_ancestors.then(|| search_root.to_path_buf());
    let floor_disclosure = filter_root
        .map(|root| crate::context::confinement::capture().for_floor_filter_root(root))
        .unwrap_or_default();
    // A disappearing child mount can make ReadDir::drop panic on ENXIO and abort
    // the daemon, so never open directories outside this walk root's filesystem.
    builder.same_file_system(true).hidden(false);
    crate::context::apply_project_ignore_rules(&mut builder, search_root)
        .parents(!crate::context::confinement::active())
        .filter_entry(move |entry| {
            let file_type = entry.file_type();
            if crate::project_content::is_excluded_with_file_type(entry.path(), file_type.as_ref())
            {
                floor_disclosure.record_source_exclusion(entry.path());
                return false;
            }
            if !crate::bash_background::registry::safe_task_entry(entry.path()) {
                return false;
            }
            let is_dir = file_type.as_ref().is_some_and(std::fs::FileType::is_dir);
            if entry.depth() > 0 && is_dir {
                match boundary
                    .as_ref()
                    .map(|boundary| boundary.should_descend(entry.path()))
                {
                    Some(Ok(false)) => {
                        skipped_foreign_mounts.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                    Some(Err(_)) => return false,
                    _ => {}
                }
            }
            let name = entry.file_name().to_string_lossy();
            let keep = if is_dir {
                !is_fallback_skipped_directory_name(&name)
            } else {
                !crate::os_metadata::is_os_metadata_file_name(entry.file_name())
            };
            if !keep {
                return false;
            }
            // Checked before the directory is opened, and only for entries
            // the walk would otherwise keep, so a denial is never counted for
            // a path the walk skips anyway. Symlinks are never followed, so
            // they need no read check.
            if let Some(confined) = &confined {
                let is_file = file_type.as_ref().is_some_and(std::fs::FileType::is_file);
                if entry.depth() > 0
                    && (is_dir || is_file)
                    && (match &ignore_root {
                        Some(root) => confined.is_ignored_below(entry.path(), is_dir, root),
                        None => confined.is_ignored(entry.path(), is_dir),
                    } || !confined.admits(entry.path()))
                {
                    return false;
                }
            }
            !is_dir || crate::bounded_io::probe_directory(entry.path(), None).is_ok()
        });
    if ignored_target {
        // Like outline, an explicitly ignored target also honors Git rules
        // without stopping at a nested repository's boundary, even inside a
        // Git work tree. The crate never filters depth zero, but still applies
        // its own precedence and pruning to every descendant.
        builder.require_git(false);
    }
    if ignore_ancestors {
        // Naming an ignored folder overrides rules above it, not ignore files
        // inside it or the walker's built-in content and directory exclusions.
        builder.parents(false).git_global(false).git_exclude(false);
    }
    builder
}

pub(crate) fn is_fallback_skipped_directory_name(name: &str) -> bool {
    matches!(
        name,
        "node_modules"
            | "target"
            | "venv"
            | ".venv"
            | ".git"
            | "__pycache__"
            | ".tox"
            | "dist"
            | "build"
    )
}

/// Used only to decide whether an explicitly named root is ignored. Descendant
/// filtering and precedence are owned by the ignore crate's walker.
#[derive(Clone, Default)]
struct TargetIgnoreRules {
    git: Vec<Arc<ignore::gitignore::Gitignore>>,
    plain: Vec<Arc<ignore::gitignore::Gitignore>>,
    aft: Vec<Arc<ignore::gitignore::Gitignore>>,
}

impl TargetIgnoreRules {
    fn for_root(root: &Path) -> Self {
        let mut rules = Self::default();
        let (global, _) = ignore::gitignore::GitignoreBuilder::new(root).build_global();
        if !global.is_empty() {
            rules.git.push(Arc::new(global));
        }
        if let Some((repo, exclude)) = crate::commands::outline::git_info_exclude_for_target(root) {
            Self::load(&mut rules.git, &repo, &exclude);
        }
        for ancestor in root.ancestors().collect::<Vec<_>>().into_iter().rev() {
            rules = rules.for_directory(ancestor);
        }
        rules
    }

    fn load(matchers: &mut Vec<Arc<ignore::gitignore::Gitignore>>, root: &Path, file: &Path) {
        if !file.is_file() {
            return;
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
        let _ = builder.add(file);
        if let Ok(matcher) = builder.build() {
            matchers.push(Arc::new(matcher));
        }
    }

    fn for_directory(&self, directory: &Path) -> Self {
        let mut rules = self.clone();
        Self::load(&mut rules.git, directory, &directory.join(".gitignore"));
        Self::load(&mut rules.plain, directory, &directory.join(".ignore"));
        Self::load(&mut rules.aft, directory, &directory.join(".aftignore"));
        rules
    }
}

pub(crate) fn target_is_ignored(path: &Path) -> bool {
    crate::bounded_io::run(path, None, |path| Ok(target_is_ignored_unbounded(&path)))
        .unwrap_or(false)
}

fn target_is_ignored_unbounded(path: &Path) -> bool {
    let root = path.parent().unwrap_or(path);
    let rules = TargetIgnoreRules::for_root(root);
    for family in [&rules.aft, &rules.plain, &rules.git] {
        for matcher in family.iter().rev() {
            let matched = matcher.matched_path_or_any_parents(path, path.is_dir());
            if !matched.is_none() {
                return matched.is_ignore();
            }
        }
    }
    false
}

/// The crate hides rejected entries before filter_entry is called. Only an
/// empty reply needs their count: compare one directory's accepted children
/// with its immediate on-disk entries, then recurse only through accepted
/// directories. No ignored directory is opened to count its contents.
pub(crate) fn diagnose_scope_counts(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    ignored_target: bool,
) -> Option<ScopeFileCounts> {
    let filters = filters.clone();
    let filter_root = filter_root.to_path_buf();
    crate::bounded_io::run(search_root, None, move |search_root| {
        Ok(diagnose_scope_counts_unbounded(
            &filter_root,
            &search_root,
            &filters,
            ignored_target,
        ))
    })
    .ok()
    .flatten()
}

fn diagnose_scope_counts_unbounded(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    ignored_target: bool,
) -> Option<ScopeFileCounts> {
    let started = Instant::now();
    let skipped = Arc::new(AtomicUsize::new(0));
    let mut counts = ScopeFileCounts::default();
    let within_budget = |visited: usize| {
        visited < MAX_FALLBACK_WALK_FILES.saturating_mul(8)
            && started.elapsed() < FALLBACK_WALK_BUDGET
            && !crate::executor::current_job_cancelled()
    };
    // A starting path a confined walk may not use has nothing in scope; it is
    // checked before the file test below, which would stat it.
    if !crate::context::confinement::admits_walk_root(search_root) {
        return Some(counts);
    }
    if search_root.is_file() {
        counts.examined = 1;
        let root = search_root.parent().unwrap_or(search_root);
        if filters.matches(root, search_root) {
            counts.eligible = 1;
        } else {
            counts.filtered = 1;
        }
        return Some(counts);
    }
    let mut pending = vec![search_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if !within_budget(counts.entries_visited) {
            return None;
        }
        let mut accepted = HashSet::new();
        let Some(mut builder) = fallback_target_walk_builder(
            &directory,
            Arc::clone(&skipped),
            ignored_target,
            false,
            Some(filter_root),
        ) else {
            continue;
        };
        builder.max_depth(Some(1));
        for entry in builder.build() {
            if !within_budget(counts.entries_visited) {
                return None;
            }
            counts.entries_visited += 1;
            let entry = entry.ok()?;
            if entry.depth() > 0 {
                accepted.insert(entry.file_name().to_os_string());
            }
        }
        if skipped.load(Ordering::Relaxed) > 0 {
            return None;
        }
        for entry in std::fs::read_dir(&directory).ok()? {
            if !within_budget(counts.entries_visited) {
                return None;
            }
            counts.entries_visited += 1;
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            if kind.is_file() {
                counts.examined += 1;
            }
            if !accepted.contains(&entry.file_name()) {
                counts.ignored += 1;
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                if filters.matches(filter_root, &path) {
                    counts.eligible += 1;
                } else {
                    counts.filtered += 1;
                }
            }
        }
    }
    Some(counts)
}

/// Bounded project walk used when the trigram index is unavailable (grep/glob fallback).
pub(crate) fn bounded_fallback_walk_files(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
) -> FallbackWalkOutcome {
    bounded_fallback_walk_files_with_limits(
        filter_root,
        search_root,
        filters,
        fallback_walk_limit(),
        fallback_walk_budget(),
    )
}

pub(crate) fn bounded_fallback_walk_files_for_target(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    ignored_target: bool,
) -> FallbackWalkOutcome {
    bounded_fallback_walk_files_with_limits_target(
        filter_root,
        search_root,
        filters,
        fallback_walk_limit(),
        fallback_walk_budget(),
        ignored_target,
        false,
    )
}

pub(crate) fn bounded_glob_walk_in_named_ignored_folder(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
) -> FallbackWalkOutcome {
    bounded_fallback_walk_files_with_limits_target(
        filter_root,
        search_root,
        filters,
        fallback_walk_limit(),
        fallback_walk_budget(),
        true,
        true,
    )
}

pub(crate) fn source_first_fallback_walk_files(
    root: &Path,
    deadline: Instant,
) -> FallbackWalkOutcome {
    let mut files = Vec::new();
    let mut entries_visited = 0;
    let skipped_foreign_mounts = Arc::new(AtomicUsize::new(0));
    let mut walk_truncated = false;
    let mut walk_time_limit_reached = false;
    'phases: for source_phase in [true, false] {
        let builder = fallback_project_walk_builder(root, Arc::clone(&skipped_foreign_mounts));
        for entry in builder.build() {
            entries_visited += 1;
            if Instant::now() >= deadline {
                walk_truncated = true;
                walk_time_limit_reached = true;
                break 'phases;
            }
            if crate::executor::current_job_cancelled() {
                walk_truncated = true;
                break 'phases;
            }
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && is_fallback_source_path(entry.path()) == source_phase
            {
                files.push(entry.into_path());
            }
        }
    }
    FallbackWalkOutcome {
        files,
        walk_truncated,
        walk_time_limit_reached,
        skipped_foreign_mounts: skipped_foreign_mounts.load(Ordering::Relaxed),
        entries_visited,
    }
}

pub(crate) fn is_fallback_source_path(path: &Path) -> bool {
    use crate::parser::LangId;
    crate::parser::detect_language(path).is_some_and(|language| {
        !matches!(
            language,
            LangId::Markdown | LangId::Json | LangId::Yaml | LangId::Toml
        )
    })
}

pub(crate) fn bounded_fallback_walk_files_with_limits(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: usize,
    budget: Duration,
) -> FallbackWalkOutcome {
    bounded_fallback_walk_files_with_limits_target(
        filter_root,
        search_root,
        filters,
        max_files,
        budget,
        false,
        false,
    )
}

fn bounded_fallback_walk_files_with_limits_target(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: usize,
    budget: Duration,
    ignored_target: bool,
    ignore_ancestors: bool,
) -> FallbackWalkOutcome {
    let filter_root = filter_root.to_path_buf();
    let filters = filters.clone();
    let entry_delay = fallback_walk_entry_delay();
    let sort_metadata_delay = fallback_walk_sort_metadata_delay();
    let deadline = crate::bounded_io::deadline_before_io_timeout(
        search_root,
        budget,
        FALLBACK_WALK_RETURN_MARGIN,
    );
    crate::bounded_io::run(search_root, None, move |search_root| {
        Ok(walk_files_unbounded(
            &filter_root,
            &search_root,
            &filters,
            max_files,
            deadline,
            ignored_target,
            ignore_ancestors,
            entry_delay,
            sort_metadata_delay,
        ))
    })
    .unwrap_or_else(|_| FallbackWalkOutcome {
        files: Vec::new(),
        walk_truncated: true,
        walk_time_limit_reached: false,
        skipped_foreign_mounts: 0,
        entries_visited: 0,
    })
}

fn walk_files_unbounded(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: usize,
    deadline: Instant,
    ignored_target: bool,
    ignore_ancestors: bool,
    entry_delay: Duration,
    sort_metadata_delay: Duration,
) -> FallbackWalkOutcome {
    let skipped_foreign_mounts = Arc::new(AtomicUsize::new(0));
    // Built before the file test below, which would stat the starting path.
    let Some(builder) = fallback_target_walk_builder(
        search_root,
        Arc::clone(&skipped_foreign_mounts),
        ignored_target,
        ignore_ancestors,
        Some(filter_root),
    ) else {
        return FallbackWalkOutcome {
            files: Vec::new(),
            walk_truncated: false,
            walk_time_limit_reached: false,
            skipped_foreign_mounts: 0,
            entries_visited: 0,
        };
    };
    let filter_root = if filter_root == search_root && search_root.is_file() {
        search_root.parent().unwrap_or(search_root)
    } else {
        filter_root
    };
    let mut files = Vec::new();
    let mut walk_truncated = false;
    let mut walk_time_limit_reached = false;
    let mut entries_visited = 0usize;
    let mut walker = builder.build();

    loop {
        // Stop between entries so a progressing walk can return before the
        // bounded I/O helper deadline. A blocked `next` still times out there.
        if Instant::now() >= deadline {
            walk_truncated = true;
            walk_time_limit_reached = true;
            break;
        }
        let Some(entry) = walker.next() else {
            break;
        };
        entries_visited += 1;
        if entries_visited > max_files.saturating_mul(8).max(1024) {
            walk_truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if filters.matches(filter_root, &path) {
            if files.len() >= max_files {
                walk_truncated = true;
                break;
            }
            files.push(path);
            if !entry_delay.is_zero() {
                std::thread::sleep(entry_delay);
            }
        }
    }

    if !walk_time_limit_reached {
        if !sort_metadata_delay.is_zero() {
            for _ in &files {
                std::thread::sleep(sort_metadata_delay);
            }
        }
        sort_walked_paths_by_mtime_desc(&mut files, filter_root);
    }
    FallbackWalkOutcome {
        files,
        walk_truncated,
        walk_time_limit_reached,
        skipped_foreign_mounts: skipped_foreign_mounts.load(Ordering::Relaxed),
        entries_visited,
    }
}

fn for_each_bounded_fallback_walk_file_with_limits<F>(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    project_root: &Path,
    path_exclusion: Option<GrepPathExclusion>,
    max_files: usize,
    budget: Duration,
    on_file: &mut F,
    ignored_target: bool,
) -> FallbackWalkProgress
where
    F: FnMut(&PathBuf),
{
    let filter_root = filter_root.to_path_buf();
    let project_root = project_root.to_path_buf();
    let filters = filters.clone();
    let cancellation = crate::executor::current_job_cancellation();
    let walked = crate::bounded_io::run(
        search_root,
        Some(Instant::now() + budget),
        move |search_root| {
            let mut files = Vec::new();
            let progress = walk_each_file_unbounded(
                &filter_root,
                &search_root,
                &filters,
                &project_root,
                path_exclusion,
                max_files,
                budget,
                &mut |path| files.push(path.clone()),
                ignored_target,
                cancellation.as_ref(),
            );
            Ok((files, progress))
        },
    );
    match walked {
        Ok((files, progress)) => {
            for file in files {
                on_file(&file);
            }
            progress
        }
        Err(_) => FallbackWalkProgress {
            bound: Some(WalkBound::TimeBudget),
            ..FallbackWalkProgress::default()
        },
    }
}

fn walk_each_file_unbounded<F: FnMut(&PathBuf)>(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    project_root: &Path,
    path_exclusion: Option<GrepPathExclusion>,
    max_files: usize,
    budget: Duration,
    on_file: &mut F,
    ignored_target: bool,
    cancellation: Option<&crate::executor::JobCancellation>,
) -> FallbackWalkProgress {
    let started = Instant::now();
    let mut files_seen = 0usize;
    let mut counts = ScopeFileCounts::default();
    let skipped_foreign_mounts = Arc::new(AtomicUsize::new(0));
    let Some(builder) = fallback_target_walk_builder(
        search_root,
        Arc::clone(&skipped_foreign_mounts),
        ignored_target,
        false,
        Some(filter_root),
    ) else {
        return FallbackWalkProgress::default();
    };
    let walker = builder.build();

    for entry in walker.filter_map(|entry| entry.ok()) {
        counts.entries_visited += 1;
        if cancellation.is_some_and(|token| token.cancel_requested_before_commit()) {
            return FallbackWalkProgress {
                bound: Some(WalkBound::Cancelled),
                skipped_foreign_mounts: skipped_foreign_mounts.load(Ordering::Relaxed),
                counts,
            };
        }
        if started.elapsed() >= budget {
            return FallbackWalkProgress {
                bound: Some(WalkBound::TimeBudget),
                skipped_foreign_mounts: skipped_foreign_mounts.load(Ordering::Relaxed),
                counts,
            };
        }
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        counts.examined += 1;
        let path = entry.into_path();
        if path_exclusion.is_some_and(|exclude| exclude(&path, project_root)) {
            counts.filtered += 1;
            continue;
        }
        if filters.matches(filter_root, &path) {
            counts.eligible += 1;
            files_seen += 1;
            if files_seen > max_files {
                return FallbackWalkProgress {
                    bound: Some(WalkBound::FileLimit(max_files)),
                    skipped_foreign_mounts: skipped_foreign_mounts.load(Ordering::Relaxed),
                    counts,
                };
            }
            on_file(&path);
        } else {
            counts.filtered += 1;
        }
    }
    FallbackWalkProgress {
        bound: None,
        skipped_foreign_mounts: skipped_foreign_mounts.load(Ordering::Relaxed),
        counts,
    }
}

pub fn weakest_index_status(left: IndexStatus, right: IndexStatus) -> IndexStatus {
    match (left, right) {
        (IndexStatus::Disabled, _) | (_, IndexStatus::Disabled) => IndexStatus::Disabled,
        (IndexStatus::Fallback, _) | (_, IndexStatus::Fallback) => IndexStatus::Fallback,
        (IndexStatus::Building, _) | (_, IndexStatus::Building) => IndexStatus::Building,
        (IndexStatus::Ready, IndexStatus::Ready) => IndexStatus::Ready,
    }
}

/// Hidden entry for `search_startup_bench` timing (fallback grep path).
#[doc(hidden)]
pub fn fallback_grep_bench(
    project_root: &Path,
    search_root: &Path,
    filter_root: &Path,
    pattern: &CompiledPattern,
    include: &[String],
    exclude: &[String],
    max_results: usize,
) -> GrepResult {
    let filters = build_path_filters(include, exclude).unwrap_or_default();
    fallback_grep(
        project_root,
        search_root,
        filter_root,
        pattern,
        &filters,
        max_results,
        IndexStatus::Fallback,
        None,
    )
}

fn fallback_grep(
    project_root: &Path,
    search_root: &Path,
    filter_root: &Path,
    pattern: &CompiledPattern,
    filters: &PathFilters,
    max_results: usize,
    index_status: IndexStatus,
    path_exclusion: Option<GrepPathExclusion>,
) -> GrepResult {
    fallback_grep_counted(
        project_root,
        search_root,
        filter_root,
        pattern,
        filters,
        max_results,
        index_status,
        path_exclusion,
        &mut ScopeFileCounts::default(),
        false,
        GrepScanOrder::Arrival,
    )
}

#[allow(clippy::too_many_arguments)]
fn fallback_grep_counted(
    project_root: &Path,
    search_root: &Path,
    filter_root: &Path,
    pattern: &CompiledPattern,
    filters: &PathFilters,
    max_results: usize,
    index_status: IndexStatus,
    path_exclusion: Option<GrepPathExclusion>,
    counts: &mut ScopeFileCounts,
    ignored_target: bool,
    order: GrepScanOrder,
) -> GrepResult {
    fallback_grep_with_limits_counted(
        project_root,
        search_root,
        filter_root,
        pattern,
        filters,
        max_results,
        index_status,
        path_exclusion,
        fallback_walk_limit(),
        FALLBACK_WALK_BUDGET,
        counts,
        ignored_target,
        order,
    )
}

/// [`fallback_grep`] with explicit walk limits: at most `max_files` eligible
/// files, and `budget` of wall-clock time for walking and searching them.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn fallback_grep_with_limits(
    project_root: &Path,
    search_root: &Path,
    filter_root: &Path,
    pattern: &CompiledPattern,
    filters: &PathFilters,
    max_results: usize,
    index_status: IndexStatus,
    path_exclusion: Option<GrepPathExclusion>,
    max_files: usize,
    budget: Duration,
) -> GrepResult {
    fallback_grep_with_limits_counted(
        project_root,
        search_root,
        filter_root,
        pattern,
        filters,
        max_results,
        index_status,
        path_exclusion,
        max_files,
        budget,
        &mut ScopeFileCounts::default(),
        false,
        GrepScanOrder::Arrival,
    )
}

#[allow(clippy::too_many_arguments)]
fn fallback_grep_with_limits_counted(
    project_root: &Path,
    search_root: &Path,
    filter_root: &Path,
    pattern: &CompiledPattern,
    filters: &PathFilters,
    max_results: usize,
    index_status: IndexStatus,
    path_exclusion: Option<GrepPathExclusion>,
    max_files: usize,
    budget: Duration,
    counts: &mut ScopeFileCounts,
    ignored_target: bool,
    order: GrepScanOrder,
) -> GrepResult {
    let _io_scope = crate::bounded_io::enter_if_absent(budget);
    let total_matches = AtomicUsize::new(0);
    let files_searched = AtomicUsize::new(0);
    let files_with_matches = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let engine_capped = AtomicBool::new(false);
    let stop_after = max_results.saturating_mul(2);
    let stop_scan = Arc::new(AtomicBool::new(false));
    let scan_deadline = Instant::now() + budget;
    let job_cancellation = crate::executor::current_job_cancellation();

    let mut matches = Vec::new();
    let mut batch: Vec<PathBuf> = Vec::with_capacity(256);
    // Files for a path-ordered scan, searched one at a time after the walk.
    let mut ordered: Vec<PathBuf> = Vec::new();
    let io_budget = crate::bounded_io::current();
    let confinement = crate::context::confinement::capture();

    let flush_batch = |batch: &mut Vec<PathBuf>, matches: &mut Vec<GrepMatch>| {
        if batch.is_empty() {
            return;
        }
        let chunk = std::mem::take(batch);
        let partial: Vec<GrepMatch> = chunk
            .par_iter()
            .map_init(
                || crate::bounded_io::enter(io_budget.clone()),
                |_scope, file| {
                    crate::context::confinement::inherit(confinement.clone(), || {
                        #[cfg(test)]
                        assert_eq!(
                            crate::context::confinement::capture(),
                            confinement,
                            "grep scan worker lost the confinement fence"
                        );
                        if stop_scan.load(Ordering::Relaxed)
                            || Instant::now() >= scan_deadline
                            || job_cancellation
                                .as_ref()
                                .is_some_and(|token| token.cancel_requested_before_commit())
                        {
                            return None;
                        }
                        let file_matches = fallback_search_file(
                            file,
                            pattern,
                            max_results,
                            stop_after,
                            &total_matches,
                            &files_searched,
                            &files_with_matches,
                            &truncated,
                            &engine_capped,
                            job_cancellation.as_ref(),
                            Some(scan_deadline),
                            false,
                        );
                        if truncated.load(Ordering::Relaxed)
                            && total_matches.load(Ordering::Relaxed) >= stop_after
                        {
                            stop_scan.store(true, Ordering::Relaxed);
                        }
                        (!file_matches.is_empty()).then_some(file_matches)
                    })
                },
            )
            .filter_map(|matches| matches)
            .flatten()
            .collect();
        matches.extend(partial);
    };

    let progress = for_each_bounded_fallback_walk_file_with_limits(
        filter_root,
        search_root,
        filters,
        project_root,
        path_exclusion,
        max_files,
        budget,
        &mut |path: &PathBuf| {
            if stop_scan.load(Ordering::Relaxed) {
                return;
            }
            if order == GrepScanOrder::Path {
                ordered.push(path.clone());
                return;
            }
            batch.push(path.clone());
            if batch.len() >= 256 {
                flush_batch(&mut batch, &mut matches);
            }
        },
        ignored_target,
    );
    flush_batch(&mut batch, &mut matches);
    // A path-ordered scan searches the walked files sorted by path, in
    // batches, in parallel within a batch, and stops after the batch that
    // brings the kept lines to the cap. Each file keeps at most its share of
    // the batch's row budget and never more than the lines still missing
    // when its batch starts (a file the cap needs more lines from is searched
    // again on its own); rows are added in batch order, so the kept lines are
    // the first by path and line whatever order files finish in. Only a scan
    // stopped by its deadline, cancelled, or cut short by the walk's file
    // limit can keep different lines.
    ordered.sort();
    let batches = ordered.chunks(crate::search_index::PATH_ORDER_BATCH_FILES);
    let batch_count = batches.len();
    for (batch_index, batch) in batches.enumerate() {
        if Instant::now() >= scan_deadline
            || job_cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
        {
            engine_capped.store(true, Ordering::Relaxed);
            break;
        }
        let remaining = max_results.saturating_sub(matches.len());
        let allowance = crate::search_index::path_order_file_allowance(
            remaining,
            batch.len(),
            crate::search_index::PATH_ORDER_BATCH_ROW_BUDGET,
        );
        let search_one = |file: &PathBuf| {
            let file_matches = AtomicUsize::new(0);
            let file_truncated = AtomicBool::new(false);
            let rows = fallback_search_file(
                file,
                pattern,
                allowance,
                allowance,
                &file_matches,
                &files_searched,
                &files_with_matches,
                &file_truncated,
                &engine_capped,
                job_cancellation.as_ref(),
                Some(scan_deadline),
                false,
            );
            total_matches.fetch_add(file_matches.load(Ordering::Relaxed), Ordering::Relaxed);
            (rows, file_truncated.load(Ordering::Relaxed))
        };
        let per_file: Vec<(Vec<GrepMatch>, bool)> = match path_batch_order_for_test(search_root) {
            Some(reversed) => {
                // Tests finish the batch's files one at a time, forward or
                // reversed, to show the kept lines do not depend on it.
                let mut slots: Vec<(Vec<GrepMatch>, bool)> =
                    (0..batch.len()).map(|_| (Vec::new(), false)).collect();
                let mut positions: Vec<usize> = (0..batch.len()).collect();
                if reversed {
                    positions.reverse();
                }
                for position in positions {
                    slots[position] = search_one(&batch[position]);
                }
                slots
            }
            None => batch
                .par_iter()
                .map_init(
                    || crate::bounded_io::enter(io_budget.clone()),
                    |_scope, file| {
                        crate::context::confinement::inherit(confinement.clone(), || {
                            search_one(file)
                        })
                    },
                )
                .collect(),
        };
        let search_again = |file: usize, need: usize| {
            // The file was already counted as searched (and as matching) by
            // the batch; only its match count changes.
            let file_matches = AtomicUsize::new(0);
            let file_truncated = AtomicBool::new(false);
            let rows = fallback_search_file(
                &batch[file],
                pattern,
                need,
                need,
                &file_matches,
                &AtomicUsize::new(0),
                &AtomicUsize::new(0),
                &file_truncated,
                &engine_capped,
                job_cancellation.as_ref(),
                Some(scan_deadline),
                false,
            );
            // The batch counted `allowance + 1` matches for it.
            total_matches.fetch_add(
                file_matches
                    .load(Ordering::Relaxed)
                    .saturating_sub(allowance + 1),
                Ordering::Relaxed,
            );
            (rows, file_truncated.load(Ordering::Relaxed))
        };
        let lines_past = crate::search_index::keep_path_ordered_batch(
            &mut matches,
            max_results,
            allowance,
            per_file,
            search_again,
            |rows| crate::search_index::note_path_order_held_rows(search_root, rows),
        );
        if lines_past {
            truncated.store(true, Ordering::Relaxed);
        }
        if matches.len() >= max_results {
            if batch_index + 1 < batch_count {
                // Later files were not read.
                truncated.store(true, Ordering::Relaxed);
                engine_capped.store(true, Ordering::Relaxed);
            }
            break;
        }
    }
    *counts = progress.counts;
    let mut walk_bound = progress.bound;
    if Instant::now() >= scan_deadline {
        // Files handed to the search after the deadline were skipped, so the
        // walk is incomplete even when its own loop finished.
        walk_bound = walk_bound.or(Some(WalkBound::TimeBudget));
        engine_capped.store(true, Ordering::Relaxed);
    }

    sort_grep_matches_by_mtime_desc(&mut matches, project_root);
    let files_searched = files_searched.load(Ordering::Relaxed);

    GrepResult {
        total_matches: total_matches.load(Ordering::Relaxed),
        matches,
        files_searched,
        files_with_matches: files_with_matches.load(Ordering::Relaxed),
        index_status,
        truncated: truncated.load(Ordering::Relaxed),
        fully_degraded: true,
        engine_capped: engine_capped.load(Ordering::Relaxed),
        walk_truncated: walk_bound.is_some(),
        skipped_foreign_mounts: progress.skipped_foreign_mounts,
        missing_on_disk: 0,
        // The walk's own time budget is reported through `walk_truncated`.
        scan_deadline_reached: false,
        files_read_directly: files_searched,
        walk_bound,
    }
}

fn fallback_search_file(
    file: &PathBuf,
    pattern: &CompiledPattern,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    files_searched: &AtomicUsize,
    files_with_matches: &AtomicUsize,
    truncated: &AtomicBool,
    engine_capped: &AtomicBool,
    job_cancellation: Option<&crate::executor::JobCancellation>,
    deadline: Option<Instant>,
    authorized_task_artifact: bool,
) -> Vec<GrepMatch> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline)
        || should_stop_fallback_search(truncated, total_matches, stop_after, job_cancellation)
    {
        engine_capped.store(true, Ordering::Relaxed);
        return Vec::new();
    }

    let bytes = match crate::project_content::read_bounded(file, deadline, authorized_task_artifact)
    {
        Ok(bytes) => bytes,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::TimedOut {
                engine_capped.store(true, Ordering::Relaxed);
            }
            return Vec::new();
        }
    };
    if crate::search_index::is_binary_bytes(&bytes) {
        return Vec::new();
    }
    let Ok(content) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    files_searched.fetch_add(1, Ordering::Relaxed);

    let line_starts = line_starts(&content);
    // Start of the line after the last reported one; see `search_literal_in_text`.
    let mut reported_line_end = 0usize;
    let mut matched_this_file = false;
    let mut matches = Vec::new();

    match pattern {
        CompiledPattern::Literal(literal) => search_literal_in_text(
            file,
            &content,
            &line_starts,
            literal,
            max_results,
            stop_after,
            total_matches,
            &mut reported_line_end,
            truncated,
            engine_capped,
            &mut matched_this_file,
            &mut matches,
            job_cancellation,
            deadline,
        ),
        CompiledPattern::Regex { compiled, .. } => {
            crate::pattern_compile::for_each_line_match(
                compiled,
                content.as_bytes(),
                |match_start, match_end| {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline)
                        || should_stop_fallback_search(
                            truncated,
                            total_matches,
                            stop_after,
                            job_cancellation,
                        )
                    {
                        engine_capped.store(true, Ordering::Relaxed);
                        return false;
                    }

                    // Regex matches are not skipped ahead within a line: the
                    // engine still decides where the next non-overlapping match
                    // starts, while only the first match on each line is reported.
                    if match_start < reported_line_end {
                        return true;
                    }
                    let (line, column, line_text, next_line_start) =
                        line_details_with_next(&content, &line_starts, match_start);
                    reported_line_end = next_line_start;

                    matched_this_file = true;
                    let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                    if match_number > max_results {
                        truncated.store(true, Ordering::Relaxed);
                        return false;
                    }

                    matches.push(GrepMatch {
                        file: file.clone(),
                        line,
                        column,
                        line_text,
                        match_text: String::from_utf8_lossy(
                            &content.as_bytes()[match_start..match_end],
                        )
                        .into_owned(),
                    });
                    true
                },
            );
        }
    }

    if matched_this_file {
        files_with_matches.fetch_add(1, Ordering::Relaxed);
    }

    matches
}

fn search_literal_in_text(
    file: &Path,
    content: &str,
    line_starts: &[usize],
    literal: &LiteralSearch,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    reported_line_end: &mut usize,
    truncated: &AtomicBool,
    engine_capped: &AtomicBool,
    matched_this_file: &mut bool,
    matches: &mut Vec<GrepMatch>,
    job_cancellation: Option<&crate::executor::JobCancellation>,
    deadline: Option<Instant>,
) {
    if literal.needle.contains(&b'\n') {
        return;
    }

    let content_bytes = content.as_bytes();
    let search_content;
    let haystack = if literal.case_insensitive_ascii {
        search_content = content_bytes.to_ascii_lowercase();
        search_content.as_slice()
    } else {
        content_bytes
    };
    let finder = memchr::memmem::Finder::new(&literal.needle);
    let mut start = 0usize;
    while start <= haystack.len() {
        let Some(position) = finder.find(&haystack[start..]) else {
            break;
        };
        if deadline.is_some_and(|deadline| Instant::now() >= deadline)
            || should_stop_fallback_search(truncated, total_matches, stop_after, job_cancellation)
        {
            engine_capped.store(true, Ordering::Relaxed);
            break;
        }

        let offset = start + position;
        // Grep reports one match per line. Once a line is recorded, resume at
        // the next line start instead of scanning its remaining occurrences.
        if offset < *reported_line_end {
            start = offset + 1;
            continue;
        }
        let (line, column, line_text, next_line_start) =
            line_details_with_next(content, line_starts, offset);
        *reported_line_end = next_line_start;
        start = next_line_start.max(offset + 1);

        *matched_this_file = true;
        let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
        if match_number > max_results {
            truncated.store(true, Ordering::Relaxed);
            break;
        }

        let end = offset + literal.needle.len();
        matches.push(GrepMatch {
            file: file.to_path_buf(),
            line,
            column,
            line_text,
            match_text: String::from_utf8_lossy(&content_bytes[offset..end]).into_owned(),
        });
    }
}

fn should_stop_fallback_search(
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
    job_cancellation: Option<&crate::executor::JobCancellation>,
) -> bool {
    job_cancellation.is_some_and(|token| token.cancel_requested_before_commit())
        || (truncated.load(Ordering::Relaxed)
            && total_matches.load(Ordering::Relaxed) >= stop_after)
}

pub(crate) fn ripgrep_glob(
    search_root: &Path,
    pattern: &str,
    max_results: usize,
) -> Option<FallbackWalkOutcome> {
    let filters = build_path_filters(&[pattern.to_string()], &[]).ok()?;
    let mut outcome = bounded_fallback_walk_files(search_root, search_root, &filters);
    outcome.files.truncate(max_results);
    Some(outcome)
}

fn current_index_status(ctx: &AppContext) -> IndexStatus {
    if ctx.cache_unavailable_reason().is_some() {
        return IndexStatus::Fallback;
    }
    let Some(search_index) =
        try_read_with_budget(ctx.search_index(), INTERACTIVE_ARTIFACT_READ_BUDGET)
    else {
        return IndexStatus::Fallback;
    };
    if search_index.as_ref().is_some_and(|index| index.ready) {
        return IndexStatus::Ready;
    }

    let build_in_progress =
        try_read_with_budget(ctx.search_index_rx(), INTERACTIVE_ARTIFACT_READ_BUDGET)
            .is_some_and(|search_index_rx| search_index_rx.is_some());
    if build_in_progress || search_index.is_some() {
        IndexStatus::Building
    } else {
        IndexStatus::Fallback
    }
}

pub fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// Floor a byte index to the nearest valid `str` char boundary (never panics).
pub fn floor_char_boundary_str(content: &str, mut index: usize) -> usize {
    index = index.min(content.len());
    while index > 0 && !content.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Prefix of `content` with at most `max_bytes` UTF-8 bytes, truncated on a char boundary.
pub fn truncate_at_char_boundary(content: &str, max_bytes: usize) -> &str {
    let end = floor_char_boundary_str(content, max_bytes);
    &content[..end]
}

/// Line number, 1-based character column and printable text of the line
/// holding `offset`. The text is bounded the way grep prints it, see
/// [`crate::search_index::bounded_grep_line_text`].
pub fn line_details(content: &str, line_starts: &[usize], offset: usize) -> (u32, u32, String) {
    let (line, column, line_text, _) = line_details_with_next(content, line_starts, offset);
    (line, column, line_text)
}

/// [`line_details`] plus the byte offset where the next line starts (the end
/// of `content` for the last line).
fn line_details_with_next(
    content: &str,
    line_starts: &[usize],
    offset: usize,
) -> (u32, u32, String, usize) {
    let offset = floor_char_boundary_str(content, offset);
    let line_index = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts.get(line_index).copied().unwrap_or(0);
    // Every entry after the first in `line_starts` follows a newline, so the
    // next entry, when there is one, is one byte past this line's newline.
    let (line_end, next_line_start) = match line_starts.get(line_index + 1) {
        Some(&next) => (next - 1, next),
        None => (content.len(), content.len()),
    };
    let line_text = crate::search_index::bounded_grep_line_text(
        content[line_start..line_end].trim_end_matches('\r'),
    );
    let column = content[line_start..offset].chars().count() as u32 + 1;
    (line_index as u32 + 1, column, line_text, next_line_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep_io_fixture() -> (tempfile::TempDir, AppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for directory in 0..100 {
            let parent = root.join(format!("directory_{directory:03}"));
            std::fs::create_dir(&parent).unwrap();
            for file in 0..100 {
                std::fs::write(
                    parent.join(format!("file_{file:03}.rs")),
                    "pub fn needle() {}\n",
                )
                .unwrap();
            }
        }
        let index = crate::search_index::SearchIndex::build_with_limit(&root, 1024 * 1024);
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root),
                ..crate::config::Config::default()
            },
        );
        *ctx.search_index().write().unwrap() = Some(index);
        (dir, ctx)
    }

    fn measured_grep(ctx: &AppContext) -> (Duration, usize, usize) {
        let budget = crate::bounded_io::Budget::after(Duration::from_secs(60));
        measured_grep_with_budget(ctx, budget)
    }

    fn measured_grep_with_budget(
        ctx: &AppContext,
        budget: crate::bounded_io::Budget,
    ) -> (Duration, usize, usize) {
        let request = crate::protocol::RawRequest {
            id: "grep-io-measurement".into(),
            command: "grep".into(),
            session_id: None,
            lsp_hints: None,
            params: serde_json::json!({"pattern": "needle", "max_results": 10001}),
        };
        let started = Instant::now();
        let response = crate::bounded_io::with_budget(Some(budget.clone()), || {
            crate::commands::grep::handle_grep(&request, ctx)
        });
        let elapsed = started.elapsed();
        assert!(response.success, "{}", response.data);
        assert_eq!(
            response.data["files_searched"], 10_000,
            "the measurement must actually verify all fixture files"
        );
        assert_eq!(response.data["total_matches"], 10_000);
        let (spawns, round_trips) = budget.measurements();
        (elapsed, spawns, round_trips)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ten_thousand_file_grep_uses_directory_probes_not_per_file_helpers() {
        let (_fixture, ctx) = grep_io_fixture();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let (_, spawns, round_trips) = pool.install(|| measured_grep(&ctx));
        assert!(spawns > 0, "cold first-access probes must be counted");
        assert!(spawns <= 132 && round_trips <= 132, "100 directories plus fixed root work, not 10000 files: spawns={spawns} round_trips={round_trips}");
        *ctx.search_index().write().unwrap() = None;
        let (_, spawns, round_trips) = pool.install(|| measured_grep(&ctx));
        assert!(spawns > 0);
        assert!(spawns <= 132 && round_trips <= 132, "the fallback walk must reuse its bounded helper: spawns={spawns} round_trips={round_trips}");
    }

    // Grep reads files on rayon workers, then stats each matched file on the
    // thread that sorts the results. Called from a thread outside any rayon
    // pool, the sorting thread reads no files itself. Each directory's access
    // probe (`bounded_io::probe_directory`) must then reach it from the
    // request's shared probe map, not from its own per-thread cache, or it
    // spawns a filesystem helper thread again for every directory. In the
    // pooled tests the sorting thread reads some directories itself, how many
    // depending on rayon scheduling, which is why they failed only sometimes.
    //
    // With the default 5 s access cap, the fallback walk probes all 100 fixture
    // directories inside its own helper, which spawns nothing. What is left is
    // fixed work on the project root (its admission stat and listing, and the
    // walk's helper): at most 32 helpers, the root allowance in the pooled
    // tests' bound of 132 = 100 directories + 32. A helper per directory or
    // per file here means a directory the request already probed was probed
    // again.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ten_thousand_file_grep_sorts_matches_without_per_directory_respawns() {
        let (_fixture, ctx) = grep_io_fixture();
        *ctx.search_index().write().unwrap() = None;
        let (_, spawns, round_trips) =
            std::thread::scope(|scope| scope.spawn(|| measured_grep(&ctx)).join().unwrap());
        assert!(spawns > 0);
        assert!(spawns <= 32 && round_trips <= 32, "sorting on a thread that read no files must reuse the directory probes: spawns={spawns} round_trips={round_trips}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ten_thousand_file_grep_keeps_root_probe_budget_after_queueing() {
        // The probe is held queued for QUEUE_HOLD, then held running for
        // RUNNING_HOLD: their sum exceeds the cap, so the probe fails if queue
        // time is charged to it. Each hold alone stays well below the cap,
        // because on a loaded runner the release and the real root access that
        // follows it can take seconds; a 1.5 s margin was not enough in CI.
        const ROOT_ACCESS_CAP: Duration = Duration::from_secs(12);
        const QUEUE_HOLD: Duration = Duration::from_secs(8);
        const RUNNING_HOLD: Duration = Duration::from_secs(6);
        let (_fixture, ctx) = grep_io_fixture();
        let root = project_root(&ctx);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        for indexed in [true, false] {
            if !indexed {
                *ctx.search_index().write().unwrap() = None;
            }
            let queued = crate::bounded_io::stall_helper(&root);
            let running = crate::bounded_io::stall_running_helper(&root);
            let budget = crate::bounded_io::Budget::after(Duration::from_secs(60))
                .with_path_access_budget(&root, ROOT_ACCESS_CAP);
            let observed = budget.clone();
            std::thread::scope(|scope| {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let pool = &pool;
                let ctx = &ctx;
                let caller = scope.spawn(move || {
                    let result = pool.install(|| measured_grep_with_budget(ctx, budget));
                    let _ = done_tx.send(());
                    result
                });
                queued
                    .reached
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
                // The first helper is command admission's metadata(root), before
                // the root listing or any indexed/fallback candidate reads.
                let cold_measurements = observed.measurements();
                let before_start = done_rx.recv_timeout(QUEUE_HOLD);
                drop(queued);
                let started = running.reached.recv_timeout(Duration::from_secs(30));
                let before_release = done_rx.recv_timeout(RUNNING_HOLD);
                drop(running);
                let (_, spawns, round_trips) = caller.join().unwrap();
                assert_eq!(cold_measurements.1, 1);
                assert!(before_start.is_err());
                started.expect("the fixture-root metadata helper must start");
                assert!(before_release.is_err());
                // In the fallback pass this test's 12 s root cap outlasts the
                // 5 s default cap of each subdirectory probe, so the walk's
                // helper on the root cannot run those probes itself (see
                // `run_with_timeout`) and each of the 100 directories gets its
                // own helper. That is still one helper per directory.
                assert!(spawns <= 132 && round_trips <= 132,
                    "indexed={indexed}: directory probes, not per-file helpers: spawns={spawns} round_trips={round_trips}");
            });
        }
    }

    #[test]
    #[ignore = "manual five-run 10000-file grep wall-time and helper-count measurement"]
    fn grep_io_10k_measurement() {
        let (_fixture, ctx) = grep_io_fixture();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        for indexed in [true, false] {
            if !indexed {
                *ctx.search_index().write().unwrap() = None;
            }
            let mut samples = Vec::new();
            for run in 0..5 {
                let (elapsed, spawns, round_trips) = pool.install(|| measured_grep(&ctx));
                println!("grep_io_10k mode={} run={} elapsed_ms={:.3} thread_spawns={} helper_round_trips={}",
                    if indexed { "indexed" } else { "fallback" }, run + 1, elapsed.as_secs_f64() * 1000.0, spawns, round_trips);
                samples.push(elapsed);
            }
            samples.sort();
            println!(
                "grep_io_10k mode={} median_ms={:.3}",
                if indexed { "indexed" } else { "fallback" },
                samples[2].as_secs_f64() * 1000.0
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn grep_scan_deadline_bounds_blocked_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.txt");
        std::fs::write(&path, "released\n").unwrap();
        let fifo = dir.path().join("consent-probe");
        crate::bounded_io::tests::fifo(&fifo);
        let probe = crate::bounded_io::tests::block_directory_probe(dir.path(), &fifo);
        let (tx, rx) = std::sync::mpsc::channel();
        let input = path.clone();
        let thread = std::thread::spawn(move || {
            let total = AtomicUsize::new(0);
            let searched = AtomicUsize::new(0);
            let matched = AtomicUsize::new(0);
            let truncated = AtomicBool::new(false);
            let capped = AtomicBool::new(false);
            let matches = fallback_search_file(
                &input,
                &literal("released"),
                100,
                200,
                &total,
                &searched,
                &matched,
                &truncated,
                &capped,
                None,
                Some(Instant::now() + Duration::from_millis(80)),
                false,
            );
            tx.send((
                matches,
                searched.load(Ordering::Relaxed),
                capped.load(Ordering::Relaxed),
            ))
            .unwrap();
        });
        let before_writer = rx.recv_timeout(Duration::from_millis(800));
        crate::bounded_io::tests::release_fifo(&fifo);
        thread.join().unwrap();
        let (matches, searched, capped) =
            before_writer.expect("grep checked its scan deadline only after open returned");
        assert!(matches.is_empty());
        assert_eq!(
            probe.fired.load(Ordering::Relaxed),
            1,
            "the actual first-access probe must run"
        );
        assert_eq!(searched, 0);
        assert!(
            capped,
            "a blocked file is an incomplete search, not a zero-match answer"
        );
    }

    /// 256 files of 100,000 matching lines each: 25.6 million matching lines,
    /// of which the default store cap of 100,100 keeps the first by path.
    fn dense_lines_project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let content = "x\n".repeat(100_000);
        for index in 0..256 {
            std::fs::write(root.join(format!("dense_{index:03}.txt")), &content).unwrap();
        }
        (dir, root)
    }

    /// The rows a capped path-ordered grep of [`dense_lines_project`] keeps:
    /// every line of the first file and the first 100 of the second.
    fn dense_lines_first_rows(root: &Path) -> Vec<(PathBuf, u32)> {
        let first = root.join("dense_000.txt");
        let second = root.join("dense_001.txt");
        (1..=100_000)
            .map(|line| (first.clone(), line))
            .chain((1..=100).map(|line| (second.clone(), line)))
            .collect()
    }

    #[test]
    fn path_ordered_fallback_scan_of_dense_files_holds_at_most_its_row_budget() {
        let (_dir, root) = dense_lines_project();
        let max_results = 100_100;
        crate::search_index::path_order_held_rows_for_test::take(&root);
        let result = fallback_grep_with_limits_counted(
            &root,
            &root,
            &root,
            &literal("x"),
            &PathFilters::default(),
            max_results,
            IndexStatus::Fallback,
            None,
            10_000,
            Duration::from_secs(600),
            &mut ScopeFileCounts::default(),
            false,
            GrepScanOrder::Path,
        );
        let peak = crate::search_index::path_order_held_rows_for_test::take(&root);
        // Every file of a batch keeping all 100,100 lines the cap needs would
        // hold 25.6 million rows to keep 100,100.
        assert!(
            peak <= max_results + crate::search_index::PATH_ORDER_BATCH_ROW_BUDGET,
            "held {peak} rows at once"
        );
        assert!(result.truncated);
        let mut kept: Vec<(PathBuf, u32)> = result
            .matches
            .iter()
            .map(|row| (row.file.clone(), row.line))
            .collect();
        kept.sort();
        assert_eq!(kept, dense_lines_first_rows(&root));
    }

    fn grep_match(file: &Path, line: u32, column: u32) -> GrepMatch {
        GrepMatch {
            file: file.to_path_buf(),
            line,
            column,
            line_text: "needle".to_string(),
            match_text: "needle".to_string(),
        }
    }

    fn result(matches: Vec<GrepMatch>, truncated: bool, status: IndexStatus) -> GrepResult {
        GrepResult {
            total_matches: matches.len(),
            files_searched: matches.len(),
            files_with_matches: matches.len(),
            matches,
            index_status: status,
            truncated,
            fully_degraded: false,
            engine_capped: false,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: 0,
            scan_deadline_reached: false,
            files_read_directly: 0,
            walk_bound: None,
        }
    }

    #[test]
    fn optional_path_exclusion_controls_visible_totals_without_affecting_default_grep() {
        fn excludes_tests(path: &Path, root: &Path) -> bool {
            path.strip_prefix(root)
                .is_ok_and(|relative| relative.starts_with("tests"))
        }

        let project = tempfile::tempdir().expect("project");
        let test_file = project.path().join("tests/case.rs");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(test_file.parent().expect("test parent")).expect("test dir");
        std::fs::create_dir_all(source_file.parent().expect("source parent")).expect("source dir");
        std::fs::write(&test_file, "const NEEDLE: &str = \"needle\";\n").expect("test file");
        std::fs::write(&source_file, "pub fn needle() {}\n").expect("source file");
        let pattern = match crate::pattern_compile::compile(
            "needle",
            crate::pattern_compile::CompileOpts {
                literal: true,
                ..crate::pattern_compile::CompileOpts::default()
            },
        ) {
            crate::pattern_compile::CompileResult::Ok(pattern) => pattern,
            other => panic!("compile literal: {other:?}"),
        };

        let filters = PathFilters::default();
        let unfiltered = fallback_grep(
            project.path(),
            project.path(),
            project.path(),
            &pattern,
            &filters,
            10,
            IndexStatus::Fallback,
            None,
        );
        assert_eq!(unfiltered.total_matches, 2);
        assert_eq!(unfiltered.matches.len(), 2);

        let visible = fallback_grep(
            project.path(),
            project.path(),
            project.path(),
            &pattern,
            &filters,
            10,
            IndexStatus::Fallback,
            Some(excludes_tests),
        );
        assert_eq!(visible.total_matches, 1);
        assert_eq!(visible.matches.len(), 1);
        assert_eq!(visible.files_searched, 1);
        assert_eq!(visible.files_with_matches, 1);
        assert_eq!(visible.matches[0].file, source_file);
        assert!(!visible.truncated);
        assert!(!visible.engine_capped);
    }

    /// A tempdir holding `count` small text files, none containing `absent_token`.
    fn walk_fixture(count: usize) -> tempfile::TempDir {
        let project = tempfile::tempdir().expect("project");
        for index in 0..count {
            std::fs::write(
                project.path().join(format!("file_{index}.txt")),
                format!("line {index} — nothing to see\n"),
            )
            .expect("fixture file");
        }
        project
    }

    fn literal(pattern: &str) -> CompiledPattern {
        match crate::pattern_compile::compile(
            pattern,
            crate::pattern_compile::CompileOpts {
                literal: true,
                ..crate::pattern_compile::CompileOpts::default()
            },
        ) {
            crate::pattern_compile::CompileResult::Ok(pattern) => pattern,
            other => panic!("compile literal: {other:?}"),
        }
    }

    /// The grep tool's reply text for `result`, as `handle_grep` builds it.
    fn grep_tool_text(result: &GrepResult, project_root: &Path) -> String {
        let mut text = crate::commands::grep::grep_page_output(
            result,
            &crate::commands::grep::GrepPageRequest {
                project_root,
                include: &[],
                file_roots: &[],
                offset: 0,
                explicit_page_rows: None,
            },
        )
        .text;
        if let Some(note) = crate::commands::grep::walk_coverage_note(result) {
            text.push_str("\n\n");
            text.push_str(&note);
        }
        text
    }

    #[test]
    fn complete_fallback_walk_while_index_builds_says_every_file_was_searched() {
        let project = walk_fixture(3);
        let result = fallback_grep_with_limits(
            project.path(),
            project.path(),
            project.path(),
            &literal("absent_token"),
            &PathFilters::default(),
            10,
            IndexStatus::Building,
            None,
            10,
            FALLBACK_WALK_BUDGET,
        );
        assert_eq!(result.walk_bound, None);
        assert!(!result.walk_truncated);
        assert_eq!(result.files_read_directly, 3);

        let text = grep_tool_text(&result, project.path());
        assert_eq!(
            text,
            "Found 0 matches in 0 files [index: building; searched all 3 files directly]"
        );
    }

    #[test]
    fn bounded_fallback_walk_while_index_builds_says_it_is_incomplete() {
        let project = walk_fixture(5);
        let result = fallback_grep_with_limits(
            project.path(),
            project.path(),
            project.path(),
            &literal("absent_token"),
            &PathFilters::default(),
            10,
            IndexStatus::Building,
            None,
            2,
            FALLBACK_WALK_BUDGET,
        );
        assert_eq!(result.walk_bound, Some(WalkBound::FileLimit(2)));
        assert!(result.walk_truncated);
        assert_eq!(result.files_read_directly, 2);

        let text = grep_tool_text(&result, project.path());
        assert!(
            text.starts_with(
                "Found 0 matches in 0 files [index: building; incomplete: checked 2 files before the walk's 2-file limit]"
            ),
            "{text}"
        );
        assert!(
            text.contains("(checked 2 of more than 2 files (index building);"),
            "{text}"
        );
        assert!(
            text.contains("retry once the index is ready"),
            "the note must name an exhaustive alternative: {text}"
        );
        assert!(!text.contains("searched all"), "{text}");
    }

    #[test]
    fn walk_time_budget_marks_the_walk_incomplete() {
        let project = walk_fixture(3);
        let result = fallback_grep_with_limits(
            project.path(),
            project.path(),
            project.path(),
            &literal("absent_token"),
            &PathFilters::default(),
            10,
            IndexStatus::Building,
            None,
            10,
            Duration::ZERO,
        );
        assert_eq!(result.walk_bound, Some(WalkBound::TimeBudget));
        let text = grep_tool_text(&result, project.path());
        assert!(
            text.contains("incomplete: checked 0 files before the walk's time budget ran out"),
            "{text}"
        );
        assert!(text.contains("ran out of its time budget"), "{text}");
    }

    #[test]
    fn single_root_uses_requested_max() {
        let scope = GrepScope {
            roots: vec![ResolvedRoot {
                search_root: PathBuf::from("/project"),
                filter_root: PathBuf::from("/project"),
                use_index: true,
                is_external: false,
                ignored_target: false,
                authorized_task_artifact: false,
            }],
            multi_root: false,
            per_root_max: 10,
        };
        assert!(!scope.multi_root);
        assert_eq!(scope.per_root_max, 10);
    }

    #[test]
    fn multi_root_uses_double_per_root_max() {
        let project = tempfile::tempdir().expect("project");
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project.path().to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        let left = project.path().join("left");
        let right = project.path().join("right");
        std::fs::create_dir_all(&left).expect("left");
        std::fs::create_dir_all(&right).expect("right");
        let paths = serde_json::json!([left.display().to_string(), right.display().to_string()]);

        let scope =
            resolve_grep_scope(&ctx, Some(&paths), 10, "test", "test", None).expect("scope");

        assert!(scope.multi_root);
        assert_eq!(scope.per_root_max, 20);
    }

    #[test]
    fn bounded_fallback_walk_truncates_at_file_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for i in 0..25 {
            let path = root.join(format!("file_{i:03}.txt"));
            std::fs::write(path, "needle\n").expect("write");
        }
        let filters = build_path_filters(&["**/*.txt".to_string()], &[]).expect("filters");
        let outcome = bounded_fallback_walk_files_with_limits(
            root,
            root,
            &filters,
            20,
            Duration::from_secs(60),
        );
        assert!(outcome.walk_truncated);
        assert_eq!(outcome.files.len(), 20);
    }

    #[test]
    fn bounded_fallback_walk_small_tree_not_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "x\n").expect("write");
        std::fs::write(root.join("b.txt"), "x\n").expect("write");
        let filters = build_path_filters(&["**/*.txt".to_string()], &[]).expect("filters");
        let outcome = bounded_fallback_walk_files(root, root, &filters);
        assert!(!outcome.walk_truncated);
        assert_eq!(outcome.files.len(), 2);
    }

    #[test]
    fn fallback_project_walk_prunes_large_ignored_trees() {
        // build is also a fixed directory skip; vendor makes sure Git's own
        // directory pruning, rather than only that backstop, is exercised.
        for ignored_directory in ["build", "vendor"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir(root.join(".git")).unwrap();
            std::fs::create_dir(root.join(ignored_directory)).unwrap();
            std::fs::create_dir(root.join("src")).unwrap();
            std::fs::write(root.join(".gitignore"), format!("{ignored_directory}/\n")).unwrap();
            for index in 0..20_000 {
                std::fs::write(
                    root.join(ignored_directory).join(format!("{index}.txt")),
                    "artifact\n",
                )
                .unwrap();
            }
            for index in 0..5 {
                std::fs::write(
                    root.join("src").join(format!("{index}.rs")),
                    "fn needle() {}\n",
                )
                .unwrap();
            }
            let filters = build_path_filters(&["*.rs".to_string()], &[]).unwrap();
            let mut counts = ScopeFileCounts::default();
            let result = fallback_grep_counted(
                root,
                root,
                root,
                &literal("needle"),
                &filters,
                100,
                IndexStatus::Fallback,
                None,
                &mut counts,
                false,
                GrepScanOrder::Arrival,
            );
            assert_eq!(result.total_matches, 5, "{ignored_directory}: {result:?}");
            assert!(!result.walk_truncated, "{ignored_directory}: {result:?}");
            assert!(
                counts.entries_visited < 100,
                "{ignored_directory}: grep visited {} entries",
                counts.entries_visited
            );
            let outcome = bounded_fallback_walk_files(root, root, &filters);
            assert_eq!(outcome.files.len(), 5);
            assert!(!outcome.walk_truncated, "{ignored_directory}: {outcome:?}");
            assert!(
                outcome.entries_visited < 100,
                "{ignored_directory}: visited {} entries",
                outcome.entries_visited
            );
            let counts = diagnose_scope_counts(root, root, &filters, false).unwrap();
            assert_eq!(
                counts.ignored, 2,
                "the ignored tree and the fixed .git skip"
            );
            assert_eq!(
                counts.examined, 6,
                "five sources and .gitignore, not build artifacts"
            );
            assert!(
                counts.entries_visited < 100,
                "{ignored_directory}: exclusion probe visited {} entries",
                counts.entries_visited
            );
        }
    }

    #[test]
    fn ordinary_fallback_walk_visits_same_entries_as_original_crate_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for directory in [".git", "src", "vendor", "build"] {
            std::fs::create_dir(root.join(directory)).unwrap();
        }
        for (path, content) in [
            (".gitignore", "vendor/\n*.log\n"),
            (".aftignore", "src/private.rs\n"),
            ("src/main.rs", "source\n"),
            ("src/private.rs", "private\n"),
            ("src/trace.log", "trace\n"),
            ("vendor/generated.rs", "generated\n"),
            ("build/output.rs", "output\n"),
            (".hidden.rs", "hidden\n"),
        ] {
            std::fs::write(root.join(path), content).unwrap();
        }
        // Reproduce the pre-change builder independently, including its fixed
        // skips and default require_git setting, rather than comparing two
        // calls through the same implementation.
        let mut original = WalkBuilder::new(root);
        original
            .same_file_system(true)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .add_custom_ignore_filename(".aftignore")
            .filter_entry(|entry| {
                if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    return !matches!(
                        entry.file_name().to_str(),
                        Some(
                            "node_modules"
                                | "target"
                                | "venv"
                                | ".venv"
                                | ".git"
                                | "__pycache__"
                                | ".tox"
                                | "dist"
                                | "build"
                        )
                    );
                }
                !crate::os_metadata::is_os_metadata_file_name(entry.file_name())
            });
        let expected = original
            .build()
            .map(|entry| entry.unwrap().into_path())
            .collect::<Vec<_>>();
        let actual = fallback_project_walk_builder(root, Arc::new(AtomicUsize::new(0)))
            .build()
            .map(|entry| entry.unwrap().into_path())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(actual.contains(&root.join("src/main.rs")));
        assert!(!actual.contains(&root.join("vendor/generated.rs")));
        assert!(!actual.contains(&root.join("src/private.rs")));
        // Outside a Git repository the same .gitignore rules apply, so the
        // walk exposes exactly the file set the repository walk did (#403).
        let in_repository = actual;
        std::fs::remove_dir(root.join(".git")).unwrap();
        let mut actual = fallback_project_walk_builder(root, Arc::new(AtomicUsize::new(0)))
            .build()
            .map(|entry| entry.unwrap().into_path())
            .collect::<Vec<_>>();
        actual.sort();
        let mut without_git_dir = in_repository;
        without_git_dir.sort();
        assert_eq!(actual, without_git_dir);
        assert!(!actual.contains(&root.join("vendor/generated.rs")));
        assert!(!actual.contains(&root.join("src/private.rs")));
    }

    fn fallback_walk_set(root: &Path) -> std::collections::BTreeSet<String> {
        let outcome = bounded_fallback_walk_files(root, root, &PathFilters::default());
        assert!(!outcome.walk_truncated, "fixture walk must not truncate");
        crate::context::ignore_rules_fixture::relative_set(root, &outcome.files)
    }

    #[test]
    fn fallback_walk_honours_gitignore_in_non_git_root_like_git_root() {
        use crate::context::ignore_rules_fixture as fixture;
        let plain_dir = tempfile::tempdir().unwrap();
        let git_dir = tempfile::tempdir().unwrap();
        fixture::write(plain_dir.path(), false);
        fixture::write(git_dir.path(), true);

        let plain = fallback_walk_set(plain_dir.path());
        fixture::assert_honours_ignore_rules(&plain, "non-git grep/glob fallback walk");
        assert_eq!(plain, fallback_walk_set(git_dir.path()));
    }
    #[test]
    fn target_walk_counts_custom_ignores_and_preserves_rule_precedence() {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in [
            (".gitignore", "a.log\nb.log\n"),
            (".ignore", "!a.log\n!b.log\n"),
            (".aftignore", "a.log\n"),
            ("a.log", "ignored\n"),
            ("b.log", "visible\n"),
        ] {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        let filters = build_path_filters(&["*.log".to_string()], &[]).unwrap();
        let outcome = bounded_fallback_walk_files(dir.path(), dir.path(), &filters);
        assert_eq!(outcome.files, vec![dir.path().join("b.log")]);
        let counts = diagnose_scope_counts(dir.path(), dir.path(), &filters, false).unwrap();
        assert_eq!(counts.examined, 5);
        assert_eq!(counts.ignored, 1);
        assert_eq!(counts.filtered, 3);
        assert_eq!(counts.eligible, 1);
    }

    #[test]
    fn explicit_target_walk_keeps_ancestor_file_rules_and_default_directory_skips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("reports");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "reports/\n*.log\n!keep.log\n",
        )
        .unwrap();
        std::fs::write(root.join("keep.log"), "keep\n").unwrap();
        std::fs::write(root.join("skip.log"), "skip\n").unwrap();
        std::fs::write(root.join("nested/.gitignore"), "!child.log\n").unwrap();
        std::fs::write(root.join("nested/child.log"), "keep nested whitelist\n").unwrap();
        for name in [".git", "node_modules", "target"] {
            // A .git marker at the target itself would start a new repository
            // and legitimately stop inherited Git rules. Test its fixed skip
            // in a descendant instead, without changing the target's identity.
            let skipped = if name == ".git" {
                root.join("default-skips/.git")
            } else {
                root.join(name)
            };
            std::fs::create_dir_all(&skipped).unwrap();
            std::fs::write(skipped.join("keep.log"), "default skip\n").unwrap();
        }
        let filters = build_path_filters(&["*.log".to_string()], &[]).unwrap();
        let outcome = bounded_fallback_walk_files_for_target(&root, &root, &filters, true);
        assert_eq!(outcome.files.len(), 2, "{:?}", outcome.files);
        assert!(outcome.files.contains(&root.join("keep.log")));
        assert!(outcome.files.contains(&root.join("nested/child.log")));
        let counts = diagnose_scope_counts(&root, &root, &filters, true).unwrap();
        assert_eq!(counts.examined, 4);
        // One ignored file and the three fixed-skip directories, not their files.
        assert_eq!(counts.ignored, 4);
    }

    #[test]
    fn filter_root_is_project_for_in_project_and_search_root_for_external_unindexed() {
        let project = PathBuf::from("/project");
        let in_project = compute_filter_root(&project, Path::new("/project/src"), true, false);
        let external = compute_filter_root(&project, Path::new("/tmp/external"), false, true);
        assert_eq!(in_project, project);
        assert_eq!(external, PathBuf::from("/tmp/external"));
    }

    #[test]
    fn weakest_status_orders_disabled_fallback_building_ready() {
        assert_eq!(
            weakest_index_status(IndexStatus::Ready, IndexStatus::Building),
            IndexStatus::Building
        );
        assert_eq!(
            weakest_index_status(IndexStatus::Building, IndexStatus::Fallback),
            IndexStatus::Fallback
        );
        assert_eq!(
            weakest_index_status(IndexStatus::Fallback, IndexStatus::Disabled),
            IndexStatus::Disabled
        );
    }

    #[test]
    fn merge_dedupes_by_canonical_file_line_column() {
        let temp = tempfile::tempdir().expect("temp");
        let file = temp.path().join("file.rs");
        std::fs::write(&file, "needle").expect("write");
        let symlink = temp.path().join("link.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&file, &symlink).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&file, &symlink).expect("symlink");

        let merged = merge_grep_results(
            vec![
                result(vec![grep_match(&file, 1, 1)], false, IndexStatus::Ready),
                result(vec![grep_match(&symlink, 1, 1)], false, IndexStatus::Ready),
            ],
            temp.path(),
            10,
        );

        assert_eq!(merged.matches.len(), 1);
    }

    #[test]
    fn merge_truncated_when_child_truncated_or_pre_merge_exceeds_max() {
        let root = Path::new("/project");
        let child = merge_grep_results(
            vec![result(
                vec![grep_match(Path::new("/project/a.rs"), 1, 1)],
                true,
                IndexStatus::Ready,
            )],
            root,
            10,
        );
        assert!(child.truncated);

        let many = merge_grep_results(
            vec![
                result(
                    vec![grep_match(Path::new("/project/a.rs"), 1, 1)],
                    false,
                    IndexStatus::Ready,
                ),
                result(
                    vec![grep_match(Path::new("/project/b.rs"), 1, 1)],
                    false,
                    IndexStatus::Ready,
                ),
            ],
            root,
            1,
        );
        assert!(many.truncated);
    }

    #[test]
    fn line_details_floors_offset_inside_multibyte_char() {
        let content = "before—after";
        let starts = line_starts(content);
        let dash_byte = content.find('—').expect("em dash");
        let mid_byte = dash_byte + 1;
        assert!(!content.is_char_boundary(mid_byte));
        let (line, column, line_text) = line_details(content, &starts, mid_byte);
        assert_eq!(line, 1);
        assert_eq!(column, content[..dash_byte].chars().count() as u32 + 1);
        assert!(line_text.contains('—'));
    }

    #[test]
    fn line_details_clamps_offset_past_end() {
        let content = "short";
        let starts = line_starts(content);
        let (line, column, _) = line_details(content, &starts, content.len() + 100);
        assert_eq!(line, 1);
        assert_eq!(column, 6);
    }

    #[test]
    fn truncate_at_char_boundary_floors_mid_multibyte_at_byte_cap() {
        let mut prefix = "a".repeat(38);
        prefix.push('—');
        prefix.push_str("tail");
        assert_eq!(prefix.len(), 45);
        assert!(!prefix.is_char_boundary(40));
        let truncated = truncate_at_char_boundary(&prefix, 40);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.ends_with('a'));
        assert!(!truncated.contains('—'));
    }

    #[test]
    fn regex_byte_match_start_mid_char_does_not_panic_in_line_details() {
        use crate::pattern_compile::{CompileOpts, CompileResult};

        let content = "xy—zz";
        let starts = line_starts(content);
        let compiled = match crate::pattern_compile::compile(
            ".",
            CompileOpts {
                multi_line: false,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => compiled,
            other => panic!("expected compiled pattern, got {other:?}"),
        };
        let crate::pattern_compile::CompiledPattern::Regex { compiled, .. } = compiled else {
            panic!("expected regex pattern");
        };
        for matched in compiled.find_iter(content.as_bytes()) {
            let _ = line_details(content, &starts, matched.start());
        }
    }

    #[test]
    fn explicit_file_grep_reports_one_match_for_a_long_line_in_every_arm() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("minified.js");
        let line = format!("start {}", "needle-pad".repeat(100_000));
        std::fs::write(&file, format!("{line}\nsecond needle line\n")).expect("write file");
        let compile = |pattern: &str, literal: bool, case_insensitive: bool| {
            match crate::pattern_compile::compile(
                pattern,
                crate::pattern_compile::CompileOpts {
                    literal,
                    case_insensitive,
                    ..crate::pattern_compile::CompileOpts::default()
                },
            ) {
                crate::pattern_compile::CompileResult::Ok(compiled) => compiled,
                other => panic!("compile {pattern:?}: {other:?}"),
            }
        };
        for pattern in [
            compile("needle", true, false),
            compile("NEEDLE", true, true),
            compile("ne+dle", false, false),
        ] {
            let result = grep_explicit_file(&file, &pattern, 100, IndexStatus::Fallback);
            let found: Vec<(u32, u32)> = result
                .matches
                .iter()
                .map(|matched| (matched.line, matched.column))
                .collect();
            assert_eq!(found, vec![(1, 7), (2, 8)], "{pattern:?}");
            assert_eq!(
                result.matches[0].line_text,
                crate::search_index::bounded_grep_line_text(&line)
            );
            assert!(!result.scan_deadline_reached);
        }
    }

    #[test]
    fn regex_grep_patterns_are_line_oriented_on_explicit_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("bash-task.stderr");
        std::fs::write(&file, "\n  566 pass\nx\n").expect("write file");

        let whitespace = compiled_regex(r"^\s+[0-9]+ pass");
        let result = grep_explicit_file(&file, &whitespace, 10, IndexStatus::Fallback);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].line, 2);
        assert_eq!(result.matches[0].line_text, "  566 pass");
        assert_eq!(result.matches[0].match_text, "  566 pass");

        let negated_class = compiled_regex(r"[^x]+");
        let result = grep_explicit_file(&file, &negated_class, 10, IndexStatus::Fallback);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].line, 2);
        assert_eq!(result.matches[0].match_text, "  566 pass");
        assert!(result
            .matches
            .iter()
            .all(|matched| !matched.match_text.contains('\n')));

        let explicit_newline = compiled_regex(r"pass\nx");
        let result = grep_explicit_file(&file, &explicit_newline, 10, IndexStatus::Fallback);
        assert!(result.matches.is_empty());

        let literal_newline = match crate::pattern_compile::compile(
            "pass\nx",
            crate::pattern_compile::CompileOpts {
                literal: true,
                ..crate::pattern_compile::CompileOpts::default()
            },
        ) {
            crate::pattern_compile::CompileResult::Ok(pattern) => pattern,
            other => panic!("compile literal newline: {other:?}"),
        };
        let result = grep_explicit_file(&file, &literal_newline, 10, IndexStatus::Fallback);
        assert!(result.matches.is_empty());

        let crlf_file = dir.path().join("crlf.stderr");
        std::fs::write(&crlf_file, "\n  566 pass\r\nx\r\n").expect("write CRLF file");
        for pattern in [r"pass\r\nx", r"\s+x"] {
            let result = grep_explicit_file(
                &crlf_file,
                &compiled_regex(pattern),
                10,
                IndexStatus::Fallback,
            );
            assert!(
                result.matches.is_empty(),
                "{pattern:?}: {:?}",
                result.matches
            );
        }

        let rescan_pattern = compiled_regex(r"foo.*\nbar|hit");
        for content in ["foo hit\nbar\n", "prefix foo hit\nbar\n"] {
            std::fs::write(&file, content).expect("write cross-line case");
            let result = grep_explicit_file(&file, &rescan_pattern, 10, IndexStatus::Fallback);
            assert_eq!(result.matches.len(), 1, "{content:?}");
            assert_eq!(result.matches[0].line, 1, "{content:?}");
            assert_eq!(result.matches[0].line_text, content.lines().next().unwrap());
            assert_eq!(result.matches[0].match_text, "hit", "{content:?}");
        }
    }

    fn compiled_regex(pattern: &str) -> CompiledPattern {
        match crate::pattern_compile::compile(
            pattern,
            crate::pattern_compile::CompileOpts::default(),
        ) {
            crate::pattern_compile::CompileResult::Ok(compiled) => compiled,
            other => panic!("compile regex {pattern:?}: {other:?}"),
        }
    }

    fn grep_result_bytes(result: &GrepResult) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "matches": result.matches.iter().map(|matched| serde_json::json!({
                "file": matched.file,
                "line": matched.line,
                "column": matched.column,
                "line_text": matched.line_text,
                "match_text": matched.match_text,
            })).collect::<Vec<_>>(),
            "total_matches": result.total_matches,
            "files_searched": result.files_searched,
            "files_with_matches": result.files_with_matches,
            "index_status": result.index_status.as_str(),
            "truncated": result.truncated,
            "fully_degraded": result.fully_degraded,
            "engine_capped": result.engine_capped,
            "walk_truncated": result.walk_truncated,
        }))
        .expect("serialize grep result projection")
    }

    #[test]
    fn multi_root_shared_query_matches_per_root_query_bytes() {
        let project = tempfile::tempdir().expect("project");
        let root_names = ["api", "cli", "daemon", "worker"];
        let roots = root_names
            .iter()
            .map(|name| {
                let root = project.path().join(name);
                std::fs::create_dir_all(&root).expect("create root");
                std::fs::write(
                    root.join("service.rs"),
                    "fn needle_alpha_12() {}\nfn needle_beta_34() {}\n",
                )
                .expect("write fixture");
                std::fs::canonicalize(root).expect("canonicalize root")
            })
            .collect::<Vec<_>>();
        let pattern = compiled_regex(r"needle_(?:alpha|beta)_\d+");
        let filters = PathFilters::default();
        let params = GrepParams {
            include: Vec::new(),
            exclude: Vec::new(),
            max_results: 100,
            path_exclusion: None,
        };
        let scope = GrepScope {
            roots: roots
                .iter()
                .map(|root| ResolvedRoot {
                    search_root: root.clone(),
                    filter_root: project.path().to_path_buf(),
                    use_index: true,
                    is_external: false,
                    ignored_target: false,
                    authorized_task_artifact: false,
                })
                .collect(),
            multi_root: true,
            per_root_max: 200,
        };
        let index =
            crate::search_index::SearchIndex::build_with_limit_serial(project.path(), 1_048_576);
        let snapshot = index.snapshot();
        let expected = merge_grep_results(
            scope
                .roots
                .iter()
                .map(|root| {
                    snapshot
                        .search_grep_profiled_with_filters(
                            &pattern,
                            &filters,
                            &root.search_root,
                            scope.per_root_max,
                            None,
                        )
                        .0
                })
                .collect(),
            project.path(),
            params.max_results,
        );
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project.path().to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        *ctx.search_index().write().expect("lock search index") = Some(index);

        let (actual, phases) =
            execute_profiled_with_filters(&ctx, &pattern, &scope, &params, &filters);

        assert_eq!(
            grep_result_bytes(&actual),
            grep_result_bytes(&expected),
            "shared query search must preserve the legacy per-root result bytes"
        );
        assert!(
            !phases.query_decomposition.is_zero(),
            "the indexed request must record its one-time query decomposition"
        );
    }

    /// Manual release-mode probe for a warm indexed TypeScript corpus searched across four roots.
    #[test]
    #[ignore = "manual release-mode issue #219 multi-root query performance probe"]
    fn issue_219_multi_root_query_reuse_perf_probe() {
        const ROOTS: usize = 4;
        const FILES_PER_ROOT: usize = 1_000;
        const SAMPLES: usize = 9;
        const ITERATIONS: usize = 300;

        let project_root = PathBuf::from("/tmp/aft-issue-219-multi-root");
        let mut index = crate::search_index::SearchIndex::new();
        let roots = (0..ROOTS)
            .map(|root_index| project_root.join(format!("packages/root-{root_index}")))
            .collect::<Vec<_>>();
        for root in &roots {
            for file_index in 0..FILES_PER_ROOT {
                index.index_file(
                    &root.join(format!("src/module-{file_index:04}.ts")),
                    b"export const indexed_value = 'warm corpus';\n",
                );
            }
        }
        let snapshot = index.snapshot();
        let pattern =
            compiled_regex(r"(?:(?:parse|format|validate)_[A-Za-z0-9_]+_)?issue_219_never_present");
        let filters = PathFilters::default();

        let per_root_once = || {
            for root in &roots {
                let result = snapshot
                    .search_grep_profiled_with_filters(&pattern, &filters, root, 100, None)
                    .0;
                std::hint::black_box(result.total_matches);
            }
        };
        let shared_query_once = || {
            let query = decompose_grep_pattern(&pattern);
            for root in &roots {
                let result = snapshot
                    .search_grep_profiled_with_filters_and_query(
                        &pattern, &query, &filters, root, 100, None,
                    )
                    .0;
                std::hint::black_box(result.total_matches);
            }
        };

        let mut per_root_ns = Vec::with_capacity(SAMPLES);
        let mut shared_query_ns = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let measure = |operation: &dyn Fn()| {
                let started = Instant::now();
                for _ in 0..ITERATIONS {
                    operation();
                }
                started.elapsed().as_nanos() / ITERATIONS as u128
            };
            if sample % 2 == 0 {
                per_root_ns.push(measure(&per_root_once));
                shared_query_ns.push(measure(&shared_query_once));
            } else {
                shared_query_ns.push(measure(&shared_query_once));
                per_root_ns.push(measure(&per_root_once));
            }
        }
        per_root_ns.sort_unstable();
        shared_query_ns.sort_unstable();
        let per_root_median = per_root_ns[SAMPLES / 2];
        let shared_query_median = shared_query_ns[SAMPLES / 2];
        let speedup = per_root_median as f64 / shared_query_median as f64;

        eprintln!(
            "issue #219 multi-root regex query: roots={ROOTS} files_per_root={FILES_PER_ROOT} samples={SAMPLES} iterations={ITERATIONS}"
        );
        eprintln!("per-root decomposition ns/op samples: {per_root_ns:?}");
        eprintln!("shared decomposition ns/op samples: {shared_query_ns:?}");
        eprintln!(
            "median: per-root={per_root_median}ns shared={shared_query_median}ns speedup={speedup:.2}x"
        );
    }
}
