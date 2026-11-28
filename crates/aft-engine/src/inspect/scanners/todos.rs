//! TODO markers in comments.
//!
//! The index store finds a file's markers when it parses the file, with the
//! rules here ([`markers_from_tree`], [`markers_lexical`]), and keeps them in
//! the file's content record. Counting them ([`run_todos_scan`]) reads the
//! records of the root's bound checkout and never parses a file: a file
//! whose record is not there yet makes the count pending, and a root with no
//! records at all reports the count unavailable.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use aft_store::content::{TodoMarker, TodoMarkers};

use crate::inspect::job::{is_test_tree_file, ExcludedTestTally};
use crate::inspect::{InspectJob, InspectResult, InspectScanSuccess};
pub(crate) use crate::parser::todo_markers::*;

pub(crate) const MAX_ITEMS: usize = 100;
/// Marker totals without any retained marker text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TodoCounts {
    pub(crate) total: usize,
    pub(crate) by_kind: [usize; 5],
}

impl TodoCounts {
    pub(crate) fn of(items: &[TodoMarker]) -> Self {
        let mut counts = Self {
            total: items.len(),
            ..Self::default()
        };
        for item in items {
            if let Some(index) = MARKERS.iter().position(|marker| *marker == item.marker) {
                counts.by_kind[index] += 1;
            }
        }
        counts
    }
}

/// What the count found for one file of its scope.
#[derive(Debug)]
pub(crate) enum FileTodos {
    /// Not read: plain JSON, or content that is not text.
    NotScanned,
    /// Read, with these markers.
    Scanned(Vec<TodoMarker>),
    /// Counted from a compact memo; items hold only the TODO detail page.
    Counted {
        counts: TodoCounts,
        items: Vec<TodoMarker>,
    },
    /// Not read, for this agent-visible reason.
    Skipped(&'static str),
}

/// What the count found for its scope from the index store.
#[derive(Debug)]
pub(crate) enum StoredTodos {
    /// Each scope file's markers, in scope order, and the repositories of a
    /// folder of repositories that have no checkout yet (relative to the
    /// folder), whose files the count leaves out.
    Files {
        files: Vec<FileTodos>,
        unindexed: Vec<String>,
    },
    /// Some file has no record yet: the store is building or catching up.
    Pending,
    /// The root has no records at all, for this agent-visible reason.
    Unavailable(&'static str),
}

/// What a file's stored markers make of it in the count: `language` is the
/// one its comments were read as, which must be the one its path calls for.
pub(crate) fn file_todos(path: &Path, todos: TodoMarkers) -> Option<FileTodos> {
    let expected = if is_header(path) {
        header_matches(&todos.language)
    } else {
        path_todo_language(path).is_some_and(|language| language.name() == todos.language)
    };
    if !expected {
        return None;
    }
    Some(match todos.items {
        Some(items) => FileTodos::Scanned(items),
        None => FileTodos::NotScanned,
    })
}

pub(crate) fn counted_file_todos(
    path: &Path,
    summary: crate::data_layer::checkout::TodoSummary,
) -> Option<FileTodos> {
    let language = summary.language?;
    let expected = if is_header(path) {
        header_matches(&language.name())
    } else {
        path_todo_language(path) == Some(language)
    };
    expected.then(|| match summary.counts {
        Some(counts) => FileTodos::Counted {
            counts,
            items: Vec::new(),
        },
        None => FileTodos::NotScanned,
    })
}

pub fn run_todos_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    let cancellation = crate::executor::current_job_cancellation();
    let cancelled = || {
        !job.project_root.is_dir()
            || cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
    };
    if cancelled() {
        return InspectResult::failed(
            job,
            "TODO scan cancelled: root unbound or missing",
            started.elapsed(),
        );
    }
    let stored = match crate::inspect::data_sources::stored_todos(
        &job.project_root,
        &job.config,
        &job.scope_files,
    ) {
        Ok(stored) => stored,
        Err(error) => return InspectResult::failed(job, error, started.elapsed()),
    };
    #[cfg(any(test, feature = "test-support"))]
    note_outcome_for_test(&job.project_root, &stored);
    if cancelled() {
        return InspectResult::failed(
            job,
            "TODO scan cancelled: root unbound or missing",
            started.elapsed(),
        );
    }
    let aggregate = match stored {
        StoredTodos::Files { files, unindexed } => {
            let mut success = aggregate(job, files);
            write_unindexed_gap(&mut success.aggregate, &unindexed);
            return InspectResult::success(job, success, started.elapsed());
        }
        // As dead code reports a call graph still building: the records
        // come with the call-graph build.
        StoredTodos::Pending => serde_json::json!({
            "status": "pending", "reason": "callgraph_building", "complete": false,
        }),
        StoredTodos::Unavailable(reason) => serde_json::json!({
            "unavailable": true,
            "complete": false,
            "gaps": [{"kind": "todo_markers_unavailable", "reason": reason}],
        }),
    };
    InspectResult::success(
        job,
        InspectScanSuccess {
            scanned_files: Vec::new(),
            contributions: Vec::new(),
            aggregate,
        },
        started.elapsed(),
    )
}

/// How many repositories with no checkout yet a gap's reason names; the
/// gap's `repositories` lists them all.
const UNINDEXED_NAMED: usize = 5;

/// Marks `aggregate` partial when some repositories of a folder of
/// repositories (`unindexed`, relative to the folder) have no checkout yet:
/// their TODOs are not counted, and the gap names them.
fn write_unindexed_gap(aggregate: &mut serde_json::Value, unindexed: &[String]) {
    if unindexed.is_empty() {
        return;
    }
    let mut names = unindexed
        .iter()
        .take(UNINDEXED_NAMED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if unindexed.len() > UNINDEXED_NAMED {
        names.push_str(&format!(", and {} more", unindexed.len() - UNINDEXED_NAMED));
    }
    let reason = format!(
        "{}: {names}",
        crate::inspect::data_sources::TODOS_REPOSITORIES_NOT_INDEXED
    );
    aggregate["complete"] = serde_json::Value::Bool(false);
    let gap = serde_json::json!({
        "kind": "analysis_incomplete",
        "reason": reason,
        "repositories": unindexed,
    });
    match aggregate["gaps"].as_array_mut() {
        Some(gaps) => gaps.push(gap),
        None => aggregate["gaps"] = serde_json::json!([gap]),
    }
}

/// The count's result for `files`, each the stored markers of the scope
/// file at the same position.
fn aggregate(job: &InspectJob, files: Vec<FileTodos>) -> InspectScanSuccess {
    let mut scanned_files = Vec::new();
    let mut skipped_files = Vec::new();
    let mut all_items = Vec::new();
    let mut excluded = ExcludedTestTally::default();
    let mut totals = TodoCounts::default();
    for (path, todos) in job.scope_files.iter().zip(files) {
        let (counts, items) = match todos {
            FileTodos::NotScanned => continue,
            FileTodos::Skipped(reason) => {
                skipped_files.push((
                    display_file_path(&job.project_root, path),
                    reason.to_string(),
                ));
                continue;
            }
            FileTodos::Scanned(items) => (TodoCounts::of(&items), items),
            FileTodos::Counted { counts, items } => (counts, items),
        };
        scanned_files.push(path.clone());
        let file = display_file_path(&job.project_root, path);
        // Markers in test trees (generated fixtures, mutation notes in test
        // comments) are not product work items; tally them separately.
        if is_test_tree_file(&file) {
            for _ in 0..counts.total {
                excluded.record(&file);
            }
        } else {
            totals.total += counts.total;
            for (total, count) in totals.by_kind.iter_mut().zip(counts.by_kind) {
                *total += count;
            }
            for item in items {
                all_items.push((file.clone(), item));
            }
        }
    }

    let mut by_kind = BTreeMap::new();
    for (marker, count) in MARKERS.into_iter().zip(totals.by_kind) {
        by_kind.insert(marker.to_string(), count);
    }

    let total_count = totals.total;
    let drill_down_capped = total_count > MAX_ITEMS;
    let items = all_items
        .into_iter()
        .take(MAX_ITEMS)
        .map(|(file, item)| {
            serde_json::json!({
                "file": file,
                "line": item.line,
                "marker": item.marker,
                "author": item.author,
                "text": item.text,
            })
        })
        .collect::<Vec<_>>();

    let mut aggregate = serde_json::json!({
        "count": total_count,
        "by_kind": by_kind,
        "items": items,
        "drill_down_capped": drill_down_capped,
    });
    excluded.write_into(&mut aggregate);
    super::source_text::write_skipped_files(&mut aggregate, skipped_files, None);
    InspectScanSuccess {
        scanned_files,
        contributions: Vec::new(),
        aggregate,
    }
}

fn display_file_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The pre-memo aggregation path, kept independent of `TodoCounts` so parity
/// checks cover both summary construction and reuse across checkout changes.
#[cfg(any(test, feature = "test-support"))]
pub fn uncached_aggregate_for_test(job: &InspectJob) -> serde_json::Value {
    let stored = crate::inspect::data_sources::stored_todos_uncached_for_test(
        &job.project_root,
        &job.config,
        &job.scope_files,
    )
    .unwrap();
    let StoredTodos::Files { files, unindexed } = stored else {
        return match stored {
            StoredTodos::Pending => serde_json::json!({
                "status": "pending", "reason": "callgraph_building", "complete": false,
            }),
            StoredTodos::Unavailable(reason) => serde_json::json!({
                "unavailable": true, "complete": false,
                "gaps": [{"kind": "todo_markers_unavailable", "reason": reason}],
            }),
            _ => unreachable!(),
        };
    };
    let mut success = aggregate_uncached_for_test(job, files);
    write_unindexed_gap(&mut success.aggregate, &unindexed);
    std::hint::black_box(success).aggregate
}

/// This test-only copy retains the full-record aggregation's scanned-files
/// collection and single-pass kind tally for baseline timing comparisons.
#[cfg(any(test, feature = "test-support"))]
fn aggregate_uncached_for_test(job: &InspectJob, files: Vec<FileTodos>) -> InspectScanSuccess {
    let mut scanned_files = Vec::new();
    let mut skipped_files = Vec::new();
    let mut all_items = Vec::new();
    let mut excluded = ExcludedTestTally::default();
    for (path, todos) in job.scope_files.iter().zip(files) {
        let items = match todos {
            FileTodos::NotScanned => continue,
            FileTodos::Skipped(reason) => {
                skipped_files.push((
                    display_file_path(&job.project_root, path),
                    reason.to_string(),
                ));
                continue;
            }
            FileTodos::Scanned(items) => items,
            FileTodos::Counted { .. } => panic!("reference must read full records"),
        };
        scanned_files.push(path.clone());
        let file = display_file_path(&job.project_root, path);
        // Markers in test trees (generated fixtures, mutation notes in test
        // comments) are not product work items; tally them separately.
        for item in items {
            if is_test_tree_file(&file) {
                excluded.record(&file);
            } else {
                all_items.push((file.clone(), item));
            }
        }
    }

    let mut by_kind = BTreeMap::new();
    for marker in MARKERS {
        by_kind.insert(marker.to_string(), 0usize);
    }
    for (_, item) in &all_items {
        if let Some(count) = by_kind.get_mut(item.marker.as_str()) {
            *count += 1;
        }
    }

    let total_count = all_items.len();
    let drill_down_capped = total_count > MAX_ITEMS;
    let items = all_items
        .into_iter()
        .take(MAX_ITEMS)
        .map(|(file, item)| {
            serde_json::json!({
                "file": file,
                "line": item.line,
                "marker": item.marker,
                "author": item.author,
                "text": item.text,
            })
        })
        .collect::<Vec<_>>();

    let mut aggregate = serde_json::json!({
        "count": total_count,
        "by_kind": by_kind,
        "items": items,
        "drill_down_capped": drill_down_capped,
    });
    excluded.write_into(&mut aggregate);
    super::source_text::write_skipped_files(&mut aggregate, skipped_files, None);
    InspectScanSuccess {
        scanned_files,
        contributions: Vec::new(),
        aggregate,
    }
}

/// What each count of a root found, in order, for tests that follow the
/// counts a background refresh takes.
#[cfg(any(test, feature = "test-support"))]
static OUTCOMES_FOR_TEST: std::sync::Mutex<Vec<(std::path::PathBuf, &'static str)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(any(test, feature = "test-support"))]
fn note_outcome_for_test(root: &Path, stored: &StoredTodos) {
    let outcome = match stored {
        StoredTodos::Files { .. } => "counted",
        StoredTodos::Pending => "pending",
        StoredTodos::Unavailable(_) => "unavailable",
    };
    OUTCOMES_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((root.to_path_buf(), outcome));
}

/// The outcomes of the counts of `root` so far: `counted`, `pending` or
/// `unavailable`.
#[cfg(any(test, feature = "test-support"))]
pub fn outcomes_for_test(root: &Path) -> Vec<&'static str> {
    OUTCOMES_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(held, _)| held == root)
        .map(|(_, outcome)| *outcome)
        .collect()
}

#[cfg(any(test, feature = "test-support"))]
pub use self::test_support::todos_job;

#[cfg(any(test, feature = "test-support"))]
mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};

    use crate::config::Config;
    use crate::inspect::{InspectCategory, InspectJob, JobKey, JobScope};
    use crate::parser::SymbolCache;

    pub fn todos_job(root: &Path, scope_files: Vec<PathBuf>) -> InspectJob {
        let scope = JobScope::for_project(root.to_path_buf());
        InspectJob {
            job_id: 1,
            key: JobKey::for_category_scope(InspectCategory::Todos, &scope),
            category: InspectCategory::Todos,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join("inspect"),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: None,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "todos_replay.rs"]
mod replay;
