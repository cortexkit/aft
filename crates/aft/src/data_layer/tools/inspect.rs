//! aft_inspect's dead-code analysis, read from the data layer.
//!
//! Dead code analyses a snapshot of the call graph: every file, exported
//! symbol, outbound call and entry point (`CallgraphSnapshot`). It comes
//! from the root's checkout once its call graph is published with no
//! changes pending; until then dead code reports the call graph as
//! building, so a change not resolved yet is never answered from an older
//! publication.
//!
//! Unused exports and the Tier-1 categories do not read the call graph.
//! Cached results are keyed by the files they were computed from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Instant;

use crate::data_layer::runtime::DataCheckout;
use crate::inspect::job::{canonicalize_normalized, normalize_path, CallgraphSnapshot};
/// How long a dead-code run waits for recorded edits to reach the call
/// graph, as the call-graph commands do.
const SNAPSHOT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// What [`DeadCodeSources::snapshot`] found for a root.
pub(crate) enum DeadCodeSnapshot {
    Ready(Arc<CallgraphSnapshot>),
    /// No checkout is bound for the root yet, or its call graph is building
    /// or catching up with edits.
    Building,
    /// The call graph failed to build, refresh or read.
    Failed(String),
}

/// The data-layer checkouts dead code reads, by the project root inspect
/// analyses. Holds each checkout weakly: the request context owns it, and a root bound to another
/// checkout drops the old one.
#[derive(Default)]
pub(crate) struct DeadCodeSources {
    checkouts: Mutex<HashMap<PathBuf, Weak<DataCheckout>>>,
}

impl DeadCodeSources {
    /// Lets dead code at `project_root` read its snapshot from `checkout`.
    /// A checkout of another folder is not used: its call graph would
    /// describe different files.
    pub(crate) fn bind(&self, project_root: &Path, checkout: &Arc<DataCheckout>) {
        if canonicalize_normalized(checkout.root()) != normalize_path(project_root) {
            crate::slog_info!(
                "tier2 dead_code: data layer not used for {}: its checkout is of {}",
                project_root.display(),
                checkout.root().display()
            );
            return;
        }
        self.checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(normalize_path(project_root), Arc::downgrade(checkout));
    }

    /// The checkout bound for `project_root`. Keys are normalized, so a root
    /// spelled with the Windows verbatim prefix (`\\?\C:\...`, what
    /// `std::fs::canonicalize` returns there) finds the same checkout as
    /// its plain spelling.
    fn checkout(&self, project_root: &Path) -> Option<Arc<DataCheckout>> {
        self.checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&normalize_path(project_root))
            .and_then(Weak::upgrade)
    }

    /// The inventory of `project_root` from its bound checkout's current
    /// view, when the view can stand in for a walk of the folder (see
    /// [`InspectInventory::from_checkout`]); `None` sends the caller to the
    /// walk.
    pub(crate) fn inventory(
        &self,
        project_root: &Path,
    ) -> Option<crate::inspect::inventory::InspectInventory> {
        let checkout = self.checkout(project_root)?;
        let view = checkout.view()?;
        crate::inspect::inventory::InspectInventory::from_checkout(project_root, &view)
    }

    /// Whether `project_root` has no call graph to analyse yet: no checkout
    /// is bound for it, or its first call graph is not published. Checked
    /// without waiting, before a run opens the inspect cache and before a
    /// cached result is served, so neither a run nor a cached answer stands
    /// in for a graph that doesn't exist yet. A published graph with edits
    /// pending is not busy: [`Self::snapshot`] brings it up to date.
    pub(crate) fn callgraph_busy(&self, project_root: &Path) -> bool {
        self.checkout(project_root)
            .and_then(|checkout| checkout.view())
            .is_none_or(|view| !view.status().callgraph_published)
    }

    /// Waits up to [`SNAPSHOT_WAIT`] for `project_root`'s first call graph to
    /// be published, asking for its build first, as an exact reader waits for
    /// pending edits. Returns whether the root still has no graph: no checkout
    /// is bound, the wait ran out, or the build failed.
    pub(crate) fn wait_for_first_callgraph(&self, project_root: &Path) -> bool {
        let Some(checkout) = self.checkout(project_root) else {
            return true;
        };
        match checkout.fresh_view(SNAPSHOT_WAIT) {
            crate::data_layer::runtime::Fresh::Ready(_) => false,
            crate::data_layer::runtime::Fresh::Building
            | crate::data_layer::runtime::Fresh::Failed(_) => self.callgraph_busy(project_root),
        }
    }

    /// Drops the dead-code snapshot every bound checkout's current view keeps
    /// between requests; the next request builds it again. Returns how many
    /// were held.
    pub(crate) fn release_dead_code_snapshots(&self) -> usize {
        let checkouts = self
            .checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        checkouts
            .iter()
            .filter_map(|checkout| checkout.view())
            .filter(|view| view.release_dead_code_snapshot())
            .count()
    }

    /// The dead-code snapshot of `project_root` from its checkout's view,
    /// once the view is published and its call graph has no changes
    /// pending.
    pub(crate) fn snapshot(&self, project_root: &Path) -> DeadCodeSnapshot {
        let Some(checkout) = self.checkout(project_root) else {
            return DeadCodeSnapshot::Building;
        };
        // The call graph is refreshed lazily, so ask for the edits recorded
        // since the last refresh to be resolved, as any exact reader does.
        let view = match checkout.fresh_view(SNAPSHOT_WAIT) {
            crate::data_layer::runtime::Fresh::Ready(view) => view,
            crate::data_layer::runtime::Fresh::Building => {
                let state = checkout.view().map_or_else(
                    || "nothing published yet".to_string(),
                    |view| format!("call graph {:?}", view.status().callgraph),
                );
                crate::slog_info!(
                    "tier2 dead_code: data layer still building for {}: {state}",
                    project_root.display()
                );
                return DeadCodeSnapshot::Building;
            }
            crate::data_layer::runtime::Fresh::Failed(reason) => {
                crate::slog_warn!(
                    "tier2 dead_code: data layer call graph failed for {}: {reason}",
                    project_root.display()
                );
                return DeadCodeSnapshot::Failed(reason);
            }
        };
        let started = Instant::now();
        // Each publication's dead-code graph is kept beside the family's
        // store, so later rollups over the same publication read it.
        let section_dir = checkout.checkout().family_dir().join("dead-code");
        match view.dead_code_snapshot_in(Some(section_dir)) {
            Ok(snapshot) => {
                crate::slog_info!(
                    "perf tier2_callgraph_snapshot: source=data_layer files={} entry_points={} ms={}",
                    snapshot.files.len(),
                    snapshot.entry_points.len(),
                    started.elapsed().as_millis()
                );
                DeadCodeSnapshot::Ready(snapshot)
            }
            Err(error) => {
                crate::slog_warn!(
                    "tier2 dead_code: data layer snapshot failed for {}: {error}",
                    project_root.display()
                );
                DeadCodeSnapshot::Failed(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {}

// Agent-visible reasons for the TODO count, approved by Ufuk; change them
// only with his approval.
/// Why a root whose checkout builds no call graph has no TODO count.
pub(crate) const TODOS_CALL_GRAPH_OFF: &str =
    "TODO counts need the call-graph index, which is off (indexes.callgraph)";
/// Why a root the index store never indexes has no TODO count.
pub(crate) const TODOS_NO_INDEX_STORE: &str =
    "this folder is not indexed (home folder, drive root, or all indexes off)";
/// Why a file the index store could not extract has no TODO markers.
pub(crate) const TODOS_NOT_PARSED: &str = "no TODO record for this file in the index store";
/// Why a file of a folder of repositories that is in none of them has no
/// TODO markers.
pub(crate) const TODOS_OUTSIDE_REPOSITORIES: &str = "not inside any repository in this folder";
/// Opens the reason of the gap naming the repositories of a folder of
/// repositories whose TODOs are not counted yet: they have no checkout.
pub(crate) const TODOS_REPOSITORIES_NOT_INDEXED: &str =
    "repositories in this folder not indexed yet";

/// Every checkout this process has bound, for the TODO count: it runs on
/// the inspect pool with only its job's root, and reads the root's
/// checkout, or for a folder of repositories its children's. Held weakly:
/// whoever bound a checkout owns it.
static BOUND: Mutex<Vec<Weak<DataCheckout>>> = Mutex::new(Vec::new());

/// Roots the data layer never binds (see `AppContext::data_layer_withheld`)
/// and folders of repositories, as their last configure left them,
/// normalized.
static ROOTS: Mutex<Vec<(PathBuf, RootKind)>> = Mutex::new(Vec::new());

/// How the data layer serves a configured root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootKind {
    /// The root gets a checkout of its own.
    Own,
    /// A folder of repositories: each repository in it gets a checkout.
    Repositories,
    /// The data layer never binds the root.
    Withheld,
}

/// Lets the TODO count read `checkout`.
pub(crate) fn note_bound(checkout: &Arc<DataCheckout>) {
    let mut bound = BOUND.lock().unwrap_or_else(PoisonError::into_inner);
    bound.retain(|held| held.strong_count() > 0);
    bound.push(Arc::downgrade(checkout));
}

/// Records how the data layer serves `root`, so the TODO count of a root
/// that never gets a checkout says why instead of waiting for one, and a
/// folder of repositories counts its repositories'.
pub(crate) fn note_root(root: &Path, kind: RootKind) {
    let root = normalize_path(root);
    let mut roots = ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
    roots.retain(|(held, _)| *held != root);
    roots.push((root, kind));
}

fn root_kind(root: &Path) -> RootKind {
    ROOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(held, _)| held == root)
        .map_or(RootKind::Own, |(_, kind)| *kind)
}

/// How long the TODO count waits for a bound checkout that is building or
/// catching up before it reports itself pending, as dead code waits for its
/// call graph.
const TODO_STORE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The TODO markers of `scope_files` (absolute) of `project_root`, from the
/// records of the root's bound checkout, or, for a folder of repositories,
/// of the bound checkouts of the repositories in it. Nothing is parsed: a
/// file whose record is not there yet makes the whole count pending, after
/// waiting up to [`TODO_STORE_WAIT`] for a bound checkout to catch up. A
/// root with no checkout bound yet is pending at once.
///
/// # Errors
///
/// When a record cannot be read.
pub(crate) fn stored_todos(
    project_root: &Path,
    config: &crate::config::Config,
    scope_files: &[PathBuf],
) -> Result<crate::inspect::scanners::todos::StoredTodos, String> {
    use crate::inspect::scanners::todos::StoredTodos;
    let deadline = Instant::now() + todo_store_wait(project_root);
    loop {
        let (todos, building) = stored_todos_now(project_root, config, scope_files)?;
        if !matches!(todos, StoredTodos::Pending)
            || !building
            || Instant::now() >= deadline
            || crate::executor::current_job_cancelled()
        {
            return Ok(todos);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// [`TODO_STORE_WAIT`], or for a test the wait it set for `project_root`.
fn todo_store_wait(project_root: &Path) -> std::time::Duration {
    #[cfg(test)]
    {
        let root = canonicalize_normalized(project_root);
        if let Some((_, wait)) = STORE_WAITS_FOR_TEST
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(held, _)| *held == root)
        {
            return *wait;
        }
    }
    #[cfg(not(test))]
    let _ = project_root;
    TODO_STORE_WAIT
}

#[cfg(test)]
static STORE_WAITS_FOR_TEST: Mutex<Vec<(PathBuf, std::time::Duration)>> = Mutex::new(Vec::new());

/// Makes the TODO count of `project_root` wait `wait` for a building
/// checkout instead of [`TODO_STORE_WAIT`].
#[cfg(test)]
pub(crate) fn set_todo_store_wait_for_test(project_root: &Path, wait: std::time::Duration) {
    STORE_WAITS_FOR_TEST
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((canonicalize_normalized(project_root), wait));
}

/// [`stored_todos`] without waiting, and whether a bound checkout is still
/// building or catching up.
fn stored_todos_now(
    project_root: &Path,
    config: &crate::config::Config,
    scope_files: &[PathBuf],
) -> Result<(crate::inspect::scanners::todos::StoredTodos, bool), String> {
    use crate::inspect::scanners::todos::{file_todos, FileTodos, StoredTodos};
    let root = canonicalize_normalized(project_root);
    let bound = BOUND
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .map(|checkout| (canonicalize_normalized(checkout.root()), checkout))
        .collect::<Vec<_>>();
    // The root's own checkout, newest first; or, for a folder of
    // repositories, the checkouts of the repositories in it, and the
    // repositories that have none bound yet.
    let kind = root_kind(&root);
    let own = bound
        .iter()
        .rev()
        .find(|(checkout_root, _)| *checkout_root == root);
    let mut unindexed = Vec::new();
    let mut covering = match own {
        Some(own) => vec![own.clone()],
        None if kind == RootKind::Repositories => {
            let mut children = bound
                .iter()
                .filter(|(checkout_root, _)| {
                    checkout_root.starts_with(&root) && *checkout_root != root
                })
                .cloned()
                .collect::<Vec<_>>();
            // One checkout per child root, the newest bound.
            children.reverse();
            let mut seen = Vec::new();
            children.retain(|(checkout_root, _)| {
                let first = !seen.contains(checkout_root);
                seen.push(checkout_root.clone());
                first
            });
            if let Some(session) = crate::views::parent::session_for_root(&root) {
                // Until the folder's repositories are found, which of them
                // lack a checkout is not known.
                if session.discovery().is_none() {
                    return Ok((StoredTodos::Pending, false));
                }
                for child in session.children() {
                    let child_root = canonicalize_normalized(&child.root);
                    if !seen.contains(&child_root) {
                        unindexed.push((
                            child_root,
                            child.relative.to_string_lossy().replace('\\', "/"),
                        ));
                    }
                }
            }
            children
        }
        None => Vec::new(),
    };
    let indexes = &config.indexes;
    if covering.is_empty() {
        let todos = match kind {
            RootKind::Withheld => StoredTodos::Unavailable(TODOS_NO_INDEX_STORE),
            RootKind::Own if !indexes.callgraph => StoredTodos::Unavailable(TODOS_CALL_GRAPH_OFF),
            // Not bound yet; a folder of repositories binds each one the
            // first time a query reaches it.
            RootKind::Own | RootKind::Repositories => StoredTodos::Pending,
        };
        return Ok((todos, false));
    }
    if covering
        .iter()
        .any(|(_, checkout)| !checkout.checkout().planes().callgraph)
    {
        return Ok((StoredTodos::Unavailable(TODOS_CALL_GRAPH_OFF), false));
    }
    // Longest root first, so a file goes to the deepest checkout holding it.
    covering.sort_by_key(|(checkout_root, _)| std::cmp::Reverse(checkout_root.as_os_str().len()));
    let mut files = (0..scope_files.len())
        .map(|_| None)
        .collect::<Vec<Option<FileTodos>>>();
    let mut held = vec![false; scope_files.len()];
    // A repository with no checkout yet is named as a gap; its files are
    // neither counted nor listed one by one.
    for (position, file) in scope_files.iter().enumerate() {
        let file = normalize_path(file);
        if unindexed
            .iter()
            .any(|(child_root, _)| file.starts_with(child_root))
        {
            held[position] = true;
            files[position] = Some(FileTodos::NotScanned);
        }
    }
    for (checkout_root, checkout) in &covering {
        let Some(view) = checkout.view() else {
            return Ok((StoredTodos::Pending, true));
        };
        // Records come with the call graph's build, and a rescan can find
        // changes the view does not have yet.
        if !view.status().callgraph_published || view.rescanning() {
            return Ok((StoredTodos::Pending, true));
        }
        let mut positions = Vec::new();
        let mut paths = Vec::new();
        for (position, file) in scope_files.iter().enumerate() {
            if held[position] {
                continue;
            }
            let file = normalize_path(file);
            let Ok(relative) = file.strip_prefix(checkout_root) else {
                continue;
            };
            held[position] = true;
            let Some(relative) = relative.to_str() else {
                files[position] = Some(FileTodos::NotScanned);
                continue;
            };
            positions.push(position);
            paths.push(relative.replace('\\', "/").into_bytes());
        }
        let found = checkout
            .checkout()
            .todo_markers(
                &view,
                &paths,
                crate::inspect::scanners::source_text::MAX_SOURCE_BYTES,
            )
            .map_err(|error| error.to_string())?;
        for (position, todos) in positions.into_iter().zip(found) {
            use crate::data_layer::checkout::PathTodos;
            let path = &scope_files[position];
            files[position] = Some(match todos {
                // Plain JSON is never read, whatever the store holds.
                _ if file_todos_skip(path) => FileTodos::NotScanned,
                PathTodos::Missing => return Ok((StoredTodos::Pending, true)),
                PathTodos::Absent | PathTodos::Unrecorded => FileTodos::NotScanned,
                PathTodos::TooLarge => {
                    FileTodos::Skipped(crate::inspect::scanners::source_text::TOO_LARGE)
                }
                PathTodos::Unextracted | PathTodos::Recorded(None) => {
                    FileTodos::Skipped(TODOS_NOT_PARSED)
                }
                PathTodos::Recorded(Some(todos)) => {
                    file_todos(path, todos).unwrap_or(FileTodos::Skipped(TODOS_NOT_PARSED))
                }
            });
        }
    }
    Ok((
        StoredTodos::Files {
            files: files
                .into_iter()
                .map(|todos| todos.unwrap_or(FileTodos::Skipped(TODOS_OUTSIDE_REPOSITORIES)))
                .collect(),
            unindexed: unindexed
                .into_iter()
                .map(|(_, relative)| relative)
                .collect(),
        },
        false,
    ))
}

/// Whether the count never reads the file at `path`, whatever the store
/// holds for it: plain JSON, which has no comments.
fn file_todos_skip(path: &Path) -> bool {
    crate::inspect::scanners::todos::path_todo_language(path).is_none()
}

/// A context whose index-store checkout of `root` is bound and has
/// published its call graph, so the TODO count of `root` reads records.
/// The checkout lives as long as the context.
#[cfg(test)]
pub(crate) fn bind_published_for_test(root: &Path) -> crate::context::AppContext {
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.to_path_buf()),
            ..crate::config::Config::default()
        },
    );
    crate::data_layer::surfaces::serve_text_index_for_test(&ctx);
    let checkout = ctx.data_checkout().expect("the index store binds the root");
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    while !checkout
        .view()
        .is_some_and(|view| view.status().callgraph_published)
        || checkout.checkout().status().building
    {
        assert!(Instant::now() < deadline, "the call graph never published");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    ctx
}
