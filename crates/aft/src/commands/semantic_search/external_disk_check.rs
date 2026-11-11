//! Bounded text corpus for explicit patterns in an unpublished external repository.
//!
//! Published repositories are read through `ExternalSearch` and the index store.
//! Only the unpublished pattern fallback builds an ephemeral trigram corpus:
//! it walks with a deadline, reads source before docs under a second deadline,
//! and reports incomplete coverage when either budget runs out. It never reads
//! a saved legacy index or publishes anything to the target repository.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::external_exact::classify_file;
use crate::search_index::SearchIndex;

/// Time allowed for walking the project and comparing file metadata.
const WALK_BUDGET: Duration = Duration::from_secs(2);
/// Time allowed, after the walk, for reading changed and new files.
const REREAD_BUDGET: Duration = Duration::from_secs(1);

/// Separate budgets for walking the root and reading its files.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budgets {
    pub walk: Duration,
    pub reread: Duration,
}

thread_local! {
    static BUDGETS_FOR_TEST: Cell<Option<Budgets>> = const { Cell::new(None) };
}

/// The budgets, overridable per thread by tests.
pub(super) fn budgets() -> Budgets {
    BUDGETS_FOR_TEST.with(Cell::get).unwrap_or(Budgets {
        walk: WALK_BUDGET,
        reread: REREAD_BUDGET,
    })
}

/// Coverage of the bounded walk and reads used by one pattern request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiskCheck {
    /// Files reached before the walk deadline.
    pub files_examined: usize,
    /// The latest walk reached every file under the root before its deadline.
    pub walk_complete: bool,
    /// Files admitted to the corpus or still awaiting a read.
    pub added: usize,
    /// Files whose current content the corpus holds.
    pub reread: usize,
    /// Walked files not read before the read deadline.
    pub not_reread: usize,
}

impl DiskCheck {
    /// True when the walk finished and every reached file had a read attempted.
    pub(super) fn verified(&self) -> bool {
        self.walk_complete && self.not_reread == 0
    }

    pub(super) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "complete": self.verified(),
            "files_examined": self.files_examined,
            "walk_complete": self.walk_complete,
            // Keep the public coverage shape; this corpus has no saved predecessor.
            "changed": 0,
            "content_unchanged": 0,
            "added": self.added,
            "removed": 0,
            "reread": self.reread,
            "not_reread": self.not_reread,
            "semantic_outdated": 0,
        })
    }
}

/// The ephemeral pattern index and the coverage of the reads that built it.
#[derive(Clone, Debug)]
pub(crate) struct CheckedIndex {
    /// Only the files the bounded walk read; never persisted.
    pub index: Arc<SearchIndex>,
    pub check: DiskCheck,
    /// Stable digest of the admitted paths and their observed stats, or `None`
    /// for an empty corpus; keeps ranking caches tied to the files read.
    pub applied_digest: Option<String>,
}

#[cfg(test)]
thread_local! {
    /// Maximum read attempts in one corpus build, standing in for the read
    /// deadline in tests that need a pass to stop at a known point.
    static WORK_LIMIT_FOR_TEST: Cell<Option<usize>> = const { Cell::new(None) };
}

fn work_limit() -> usize {
    #[cfg(test)]
    if let Some(limit) = WORK_LIMIT_FOR_TEST.with(Cell::get) {
        return limit;
    }
    usize::MAX
}

/// One file the walk reached.
struct WalkedFile {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    /// A read was attempted before the deadline.
    resolved: bool,
}

/// Build the unpublished external pattern corpus with bounded, source-first reads.
/// Published repositories are served by `ExternalSearch` instead; this corpus
/// starts empty on every request and is never saved to the target repository.
pub(super) fn collect_bounded_corpus(root: &Path) -> CheckedIndex {
    let budgets = budgets();
    let walk_deadline = Instant::now() + budgets.walk;
    let stop_requested =
        |deadline: Instant| Instant::now() >= deadline || crate::executor::current_job_cancelled();
    let mut check = DiskCheck::default();
    let mut walked = Vec::new();
    check.walk_complete = true;
    for entry in crate::search_index::project_walk_builder(root).build() {
        if stop_requested(walk_deadline) {
            check.walk_complete = false;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        walked.push(WalkedFile {
            path: entry.path().to_path_buf(),
            size: metadata.len(),
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            resolved: false,
        });
    }
    check.files_examined = walked.len();
    let mut work: Vec<usize> = (0..walked.len()).collect();
    work.sort_by_cached_key(|&i| (classify_file(&walked[i].path), walked[i].path.clone()));
    let deadline = Instant::now() + budgets.reread;
    let mut index = SearchIndex::empty_ready_for_root(root);
    let mut remaining = work_limit();
    for &i in &work {
        if stop_requested(deadline) || remaining == 0 {
            break;
        }
        remaining -= 1;
        index.update_file(&walked[i].path);
        walked[i].resolved = true;
    }
    let mut digest_entries = Vec::new();
    for file in &walked {
        if !file.resolved {
            check.not_reread += 1;
            check.added += 1;
        } else if index.path_to_id.contains_key(&file.path) {
            check.added += 1;
            check.reread += 1;
            digest_entries.push((&file.path, file.size, file.modified));
        }
    }
    let applied_digest = (!digest_entries.is_empty()).then(|| {
        digest_entries.sort();
        let mut digest = blake3::Hasher::new();
        for (path, size, modified) in digest_entries {
            digest.update(b"+");
            digest.update(path.as_os_str().as_encoded_bytes());
            digest.update(&size.to_le_bytes());
            digest.update(
                &modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .to_le_bytes(),
            );
        }
        digest.finalize().to_hex()[..16].to_string()
    });
    CheckedIndex {
        index: Arc::new(index),
        check,
        applied_digest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("project dir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        for (relative, content) in files {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
            std::fs::write(path, content).expect("write file");
        }
        (dir, root)
    }

    #[test]
    fn unpublished_pattern_corpus_is_bounded_and_source_first() {
        let (_dir, root) = project(&[
            ("a.md", "needle in docs\n"),
            ("src/z.rs", "pub fn needle() {}\n"),
        ]);
        WORK_LIMIT_FOR_TEST.with(|limit| limit.set(Some(1)));
        let checked = collect_bounded_corpus(&root);
        WORK_LIMIT_FOR_TEST.with(|limit| limit.set(None));

        assert!(!checked.check.verified(), "{:?}", checked.check);
        assert_eq!(checked.check.files_examined, 2);
        assert_eq!(checked.check.added, 2);
        assert_eq!(checked.check.reread, 1);
        assert_eq!(checked.check.not_reread, 1);
        assert!(checked
            .index
            .path_to_id
            .contains_key(&root.join("src/z.rs")));
        assert!(!checked.index.path_to_id.contains_key(&root.join("a.md")));
    }

    #[test]
    fn unpublished_pattern_corpus_reports_complete_coverage() {
        let (_dir, root) = project(&[("src/lib.rs", "pub fn kept_needle() {}\n")]);
        let checked = collect_bounded_corpus(&root);
        assert!(checked.check.verified(), "{:?}", checked.check);
        assert_eq!(checked.check.files_examined, 1);
        assert_eq!(checked.check.added, 1);
        assert_eq!(checked.check.reread, 1);
        assert!(checked.applied_digest.is_some());
        let data = checked.check.to_json();
        for key in [
            "changed",
            "content_unchanged",
            "removed",
            "semantic_outdated",
        ] {
            assert_eq!(data[key], 0, "{data}");
        }
    }
}
