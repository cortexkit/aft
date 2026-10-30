//! Direct query enumeration when root-wide indexes are not admitted.
//! Like ordinary grep/glob, this uses `bounded_io::run_walk` to stop at the time
//! budget, retain paths already found when a directory read stalls, and report
//! the path that stopped making progress.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Serialize;

use crate::search_index::PathFilters;

pub(crate) const BYTE_BUDGET: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stop {
    #[default]
    Complete,
    EntryBudget,
    FileBudget,
    ByteBudget,
    TimeBudget,
    Cancelled,
    IoError,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Outcome {
    #[serde(skip)]
    pub files: Vec<PathBuf>,
    pub stop: Stop,
    pub entries_visited: usize,
    pub files_examined: usize,
    pub ignored: usize,
    pub filtered: usize,
    pub errors: usize,
    pub entry_budget: usize,
    pub file_budget: usize,
    pub time_budget_ms: u128,
    pub byte_budget: usize,
    pub ignore_file_byte_budget: usize,
    pub bytes_read: usize,
}

struct Rules {
    matcher: Gitignore,
    parent: Option<Arc<Rules>>,
}

impl Rules {
    fn ignored(&self, path: &Path, directory: bool) -> bool {
        let mut current = Some(self);
        while let Some(rules) = current {
            match rules.matcher.matched_path_or_any_parents(path, directory) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => current = rules.parent.as_deref(),
            }
        }
        false
    }

    fn load(
        directory: &Path,
        parent: Option<Arc<Rules>>,
        bytes: &mut usize,
    ) -> std::io::Result<Arc<Self>> {
        let mut builder = GitignoreBuilder::new(directory);
        let mut paths = vec![
            directory.join(".gitignore"),
            directory.join(".ignore"),
            directory.join(".aftignore"),
        ];
        if parent.is_none() && !crate::context::confinement::active() {
            if let Some(global) = ignore::gitignore::gitconfig_excludes_path() {
                paths.insert(0, global);
            }
            paths.insert(0, directory.join(".git/info/exclude"));
        }
        for path in paths {
            // Ignore files may be untrusted or replaced while walking. A bounded
            // read and regular-file check keep query policy loading finite too.
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            };
            if !metadata.is_file() {
                return Err(std::io::ErrorKind::InvalidInput.into());
            }
            let remaining = BYTE_BUDGET.saturating_sub(*bytes).min(64 * 1024);
            if remaining == 0 {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            let (content, cut) = crate::bounded_io::read_limited(&path, None, remaining - 1)?;
            *bytes += content.len() + usize::from(cut);
            if cut {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            let content = std::str::from_utf8(&content).map_err(std::io::Error::other)?;
            for line in content.lines() {
                builder
                    .add_line(Some(path.clone()), line)
                    .map_err(std::io::Error::other)?;
            }
        }
        Ok(Arc::new(Self {
            matcher: builder.build().map_err(std::io::Error::other)?,
            parent,
        }))
    }
}

pub(crate) fn walk(
    filter_root: &Path,
    root: &Path,
    filters: &PathFilters,
    max_files: usize,
    budget: Duration,
    ignored_target: bool,
) -> Outcome {
    let filter_root = filter_root.to_path_buf();
    let filters = filters.clone();
    let cancellation = crate::executor::current_job_cancellation();
    let deadline = Instant::now() + budget;
    let deadline = crate::bounded_io::current().map_or(deadline, |b| {
        b.deadline
            .checked_sub(Duration::from_millis(100))
            .unwrap_or(b.deadline)
            .min(deadline)
    });
    let initial = Outcome {
        entry_budget: max_files.saturating_mul(8).max(1024),
        file_budget: max_files,
        time_budget_ms: budget.as_millis(),
        byte_budget: BYTE_BUDGET,
        ignore_file_byte_budget: 64 * 1024,
        ..Outcome::default()
    };
    let (mut outcome, completion) =
        crate::bounded_io::run_walk(root, Some(deadline), initial, move |root, progress| {
            Ok(walk_inner(
                &filter_root,
                &root,
                &filters,
                max_files,
                budget,
                ignored_target,
                cancellation.as_ref(),
                &progress,
            ))
        });
    if completion.incomplete {
        if completion.time_limit {
            outcome.stop = Stop::TimeBudget;
        } else if outcome.stop == Stop::Complete {
            outcome.stop = Stop::IoError;
        }
    }
    outcome
}

fn publish_counts(progress: &crate::bounded_io::WalkProgress<Outcome>, out: &Outcome) {
    progress.update(|snapshot| {
        snapshot.stop = out.stop;
        snapshot.entries_visited = out.entries_visited;
        snapshot.files_examined = out.files_examined;
        snapshot.ignored = out.ignored;
        snapshot.filtered = out.filtered;
        snapshot.errors = out.errors;
        snapshot.bytes_read = out.bytes_read;
    });
}

fn walk_inner(
    filter_root: &Path,
    root: &Path,
    filters: &PathFilters,
    max_files: usize,
    budget: Duration,
    ignored_target: bool,
    cancellation: Option<&crate::executor::JobCancellation>,
    progress: &crate::bounded_io::WalkProgress<Outcome>,
) -> Outcome {
    let mut out = Outcome {
        entry_budget: max_files.saturating_mul(8).max(1024),
        file_budget: max_files,
        time_budget_ms: budget.as_millis(),
        byte_budget: BYTE_BUDGET,
        ignore_file_byte_budget: 64 * 1024,
        ..Outcome::default()
    };
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(root).ok();
    let mut ancestors = None;
    if !ignored_target && root != filter_root && root.starts_with(filter_root) {
        let mut paths = root
            .ancestors()
            .skip(1)
            .take_while(|path| path.starts_with(filter_root))
            .collect::<Vec<_>>();
        paths.reverse();
        for path in paths {
            if progress.stopped() {
                out.stop = Stop::TimeBudget;
                return out;
            }
            if !crate::context::confinement::admits_walk_root(path) {
                continue;
            }
            if !crate::bounded_io::walk_path(path) {
                out.stop = Stop::TimeBudget;
                return out;
            }
            match Rules::load(path, ancestors, &mut out.bytes_read) {
                Ok(rules) => ancestors = Some(rules),
                Err(_) => {
                    out.stop = Stop::IoError;
                    out.errors += 1;
                    return out;
                }
            }
        }
    }
    let mut pending = vec![(root.to_path_buf(), ancestors)];
    while let Some((directory, parent)) = pending.pop() {
        if progress.stopped() {
            out.stop = Stop::TimeBudget;
            break;
        }
        if cancellation.is_some_and(|c| c.cancel_requested_before_commit()) {
            out.stop = Stop::Cancelled;
            break;
        }
        if !crate::context::confinement::admits_walk_root(&directory) {
            continue;
        }
        publish_counts(progress, &out);
        if !crate::bounded_io::walk_path(&directory) {
            out.stop = Stop::TimeBudget;
            break;
        }
        let metadata = if directory == root {
            std::fs::metadata(&directory)
        } else {
            std::fs::symlink_metadata(&directory)
        };
        let Ok(metadata) = metadata else {
            out.errors += 1;
            out.stop = Stop::IoError;
            continue;
        };
        if metadata.is_file() {
            out.entries_visited += 1;
            out.files_examined += 1;
            out.files.push(directory);
            progress.update(|snapshot| snapshot.files.push(out.files.last().unwrap().clone()));
            publish_counts(progress, &out);
            continue;
        }
        if metadata.file_type().is_symlink() {
            continue;
        }
        let rules = match Rules::load(&directory, parent, &mut out.bytes_read) {
            Ok(rules) => rules,
            Err(error) => {
                out.errors += 1;
                out.stop = if error.kind() == std::io::ErrorKind::FileTooLarge {
                    Stop::ByteBudget
                } else {
                    Stop::IoError
                };
                break;
            }
        };
        let mut entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                out.errors += 1;
                out.stop = Stop::IoError;
                continue;
            }
        };
        loop {
            publish_counts(progress, &out);
            if progress.stopped() {
                out.stop = Stop::TimeBudget;
                break;
            }
            if cancellation.is_some_and(|c| c.cancel_requested_before_commit()) {
                out.stop = Stop::Cancelled;
                break;
            }
            if out.entries_visited >= out.entry_budget {
                out.stop = Stop::EntryBudget;
                break;
            }
            let Some(entry) = entries.next() else {
                break;
            };
            out.entries_visited += 1;
            publish_counts(progress, &out);
            let Ok(entry) = entry else {
                out.errors += 1;
                out.stop = Stop::IoError;
                continue;
            };
            let path = entry.path();
            if !crate::bounded_io::walk_path(&path) {
                out.stop = Stop::TimeBudget;
                break;
            }
            let Ok(kind) = entry.file_type() else {
                out.errors += 1;
                out.stop = Stop::IoError;
                continue;
            };
            // Charge every entry before exclusions or include patterns. A flat
            // directory full of ignored/nonmatching names must still hit a bound.
            if kind.is_symlink()
                || crate::project_content::is_excluded_with_file_type(&path, Some(&kind))
                || !crate::context::confinement::admits_walk_root(&path)
                || !crate::bash_background::registry::safe_task_entry(&path)
                || rules.ignored(&path, kind.is_dir())
            {
                out.ignored += 1;
                continue;
            }
            if kind.is_dir() {
                if boundary
                    .as_ref()
                    .is_none_or(|b| b.should_descend(&path).ok() != Some(true))
                {
                    out.errors += 1;
                    out.stop = Stop::IoError;
                    continue;
                }
                pending.push((path, Some(Arc::clone(&rules))));
            } else if kind.is_file() {
                out.files_examined += 1;
                if !filters.matches(filter_root, &path) {
                    out.filtered += 1;
                    continue;
                }
                if out.files.len() >= max_files {
                    out.stop = Stop::FileBudget;
                    break;
                }
                out.files.push(path);
                progress.update(|snapshot| snapshot.files.push(out.files.last().unwrap().clone()));
            }
        }
        if matches!(
            out.stop,
            Stop::EntryBudget
                | Stop::FileBudget
                | Stop::TimeBudget
                | Stop::Cancelled
                | Stop::ByteBudget
        ) {
            break;
        }
    }
    publish_counts(progress, &out);
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn folder_admission_bounded_walk_retains_prefix_and_names_a_stall() {
        let root = tempfile::tempdir().unwrap();
        let found = root.path().join("found.rs");
        let slow = root.path().join("slow");
        std::fs::write(&found, "needle\n").unwrap();
        std::fs::create_dir(&slow).unwrap();
        let stall = crate::bounded_io::stall_running_helper(&slow);
        let budget = crate::bounded_io::Budget::after(Duration::from_secs(2))
            .with_path_access_budget(&slow, Duration::from_millis(100));
        let response = crate::bounded_io::with_budget(Some(budget), || {
            let walked = walk(
                root.path(),
                root.path(),
                &PathFilters::default(),
                100,
                Duration::from_secs(2),
                false,
            );
            assert_eq!(walked.files, [found.clone()], "{walked:?}");
            assert_ne!(walked.stop, Stop::Complete);
            crate::bounded_io::finish_response(crate::protocol::Response::success(
                "bounded-folder-stall",
                serde_json::json!({"complete": true, "files": walked.files}),
            ))
        });
        assert!(response.success, "{}", response.data);
        assert_eq!(response.data["complete"], false);
        assert!(
            response.data["text"]
                .as_str()
                .unwrap()
                .contains(slow.to_str().unwrap()),
            "{}",
            response.data
        );
        drop(stall);
    }
}
