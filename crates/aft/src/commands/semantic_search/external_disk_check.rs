//! Checking another project's saved search index against its files on disk.
//!
//! `aft_search` with a `path` in another project answers from that project's
//! saved AFT index when one exists. Nothing in this session refreshes that
//! index, and opening it only parses the saved file, so a file edited, added
//! or deleted since the index was saved would otherwise be answered from what
//! it held then, or not at all.
//!
//! Before such an index answers a query, [`check_against_disk`] walks the
//! project once, checking a deadline at every walk entry, and compares each
//! file's size and modification time with the index's record of it:
//!
//! - unchanged files keep the saved postings and are not read;
//! - changed and new files are read from disk into an in-memory copy of the
//!   index (the saved file is never written), source files first, under a
//!   second deadline;
//! - files the index lists but the finished walk did not find are dropped.
//!
//! The returned [`DiskCheck`] says how many files were examined and whether the
//! walk and the re-reading finished, so the reply can say whether its answer
//! reflects every file on disk or only part of the project.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::external_exact::classify_file;
use crate::search_index::SearchIndex;
use crate::semantic_index::SemanticIndex;

/// Time allowed for walking the project and comparing file metadata.
const WALK_BUDGET: Duration = Duration::from_secs(2);
/// Time allowed, after the walk, for reading changed and new files.
const REREAD_BUDGET: Duration = Duration::from_secs(1);
/// How long a finished check keeps answering later queries for the same
/// saved index before the project is walked again. Agents often send several
/// queries to another project in a row; each reply that reuses a check says
/// how old it is.
const REUSE_WINDOW: Duration = Duration::from_secs(12);
/// Most projects whose checked index is kept at once.
const MAX_CHECKED_ROOTS: usize = 8;

/// Walk budget, re-read budget and reuse window.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budgets {
    pub walk: Duration,
    pub reread: Duration,
    pub reuse_window: Duration,
}

thread_local! {
    static BUDGETS_FOR_TEST: Cell<Option<Budgets>> = const { Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    static WALKS_FOR_TEST: Cell<usize> = const { Cell::new(0) };
}

/// The budgets, overridable per thread by tests.
pub(super) fn budgets() -> Budgets {
    BUDGETS_FOR_TEST.with(Cell::get).unwrap_or(Budgets {
        walk: WALK_BUDGET,
        reread: REREAD_BUDGET,
        reuse_window: REUSE_WINDOW,
    })
}

/// Run `run` with the walk and re-read budgets replaced on this thread. The
/// reuse window is zero so every query checks the disk afresh.
#[cfg(test)]
pub(crate) fn with_budgets_for_test<R>(
    walk: Duration,
    reread: Duration,
    run: impl FnOnce() -> R,
) -> R {
    with_all_budgets_for_test(
        Budgets {
            walk,
            reread,
            reuse_window: Duration::ZERO,
        },
        run,
    )
}

#[cfg(test)]
pub(crate) fn with_all_budgets_for_test<R>(budgets: Budgets, run: impl FnOnce() -> R) -> R {
    BUDGETS_FOR_TEST.with(|slot| {
        let previous = slot.replace(Some(budgets));
        let result = run();
        slot.set(previous);
        result
    })
}

/// How many project walks [`check_against_disk`] has started on this thread.
#[cfg(test)]
pub(crate) fn walks_for_test() -> usize {
    WALKS_FOR_TEST.with(Cell::get)
}

/// What comparing a saved index with the files on disk found. Every count
/// describes the checked copy that answers the query, measured against the
/// saved index, so a copy completed over several passes reports all of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiskCheck {
    /// Files the latest walk reached and compared with the copy.
    pub files_examined: usize,
    /// The latest walk reached every file under the root before its deadline.
    pub walk_complete: bool,
    /// Saved files whose bytes on disk differ from the saved index; the copy
    /// holds their current content.
    pub changed: usize,
    /// Saved files whose modification time differs from the saved index but
    /// whose bytes, hashed, are the same; their saved postings still answer.
    pub content_unchanged: usize,
    /// Files on disk the saved index has no record of.
    pub added: usize,
    /// Saved files the finished walk did not find. Only counted when the walk
    /// completed: an unfinished walk cannot tell missing from unvisited.
    pub removed: usize,
    /// Changed or new files whose current content the copy holds.
    pub reread: usize,
    /// Files whose size or modification time differs from the copy and that
    /// were neither hashed nor read before the deadline; the copy still
    /// answers for them as the saved index does (or, for new files, does not
    /// know them).
    pub not_reread: usize,
    /// Examined files the saved semantic index describes with older content,
    /// plus new files it has no vectors for. Zero when no semantic index was
    /// given.
    pub semantic_outdated: usize,
}

impl DiskCheck {
    /// True when every file on disk was compared and every difference was
    /// read, so the checked index answers exactly as the files are now.
    pub(super) fn verified(&self) -> bool {
        self.walk_complete && self.not_reread == 0
    }

    pub(super) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "complete": self.verified(),
            "files_examined": self.files_examined,
            "walk_complete": self.walk_complete,
            "changed": self.changed,
            "content_unchanged": self.content_unchanged,
            "added": self.added,
            "removed": self.removed,
            "reread": self.reread,
            "not_reread": self.not_reread,
            "semantic_outdated": self.semantic_outdated,
        })
    }
}

/// A saved index brought in line with the disk as far as the budgets allowed.
#[derive(Clone, Debug)]
pub(crate) struct CheckedIndex {
    /// The saved index itself when nothing differed, otherwise an in-memory
    /// copy with the differences applied.
    pub index: Arc<SearchIndex>,
    pub check: DiskCheck,
    /// Stable digest of the differences that were applied, `None` when none
    /// were. Two queries over the same unchanged files get the same digest, so
    /// caches keyed by index generation stay usable without mixing versions.
    pub applied_digest: Option<String>,
    /// Time spent walking the project and comparing metadata.
    pub walk_time: Duration,
    /// Time spent reading changed and new files.
    pub reread_time: Duration,
}

/// One project's checked index, kept for [`REUSE_WINDOW`].
#[derive(Debug)]
struct CheckedOverlay {
    root: PathBuf,
    /// Generation of the saved index the check started from.
    generation: String,
    /// The saved index and semantic index the check compared; a later query
    /// reuses the check only while it is answered from these same indexes.
    saved: Weak<SearchIndex>,
    semantic: Option<Weak<SemanticIndex>>,
    checked: CheckedIndex,
    checked_at: Instant,
}

/// Recently checked indexes of other projects, at most one per project and
/// [`MAX_CHECKED_ROOTS`] projects, least recently used dropped first. The
/// owner drops a project's entry whenever it drops or replaces that project's
/// borrowed index, so a kept copy never outlives the index it was made from.
#[derive(Debug, Default)]
pub(crate) struct CheckedOverlays {
    entries: VecDeque<CheckedOverlay>,
}

/// What [`CheckedOverlays::lookup`] found for a project.
pub(crate) enum OverlayLookup {
    /// A check made less than the reuse window ago from these same indexes,
    /// with its age: answer from it without walking.
    Reuse(CheckedIndex, Duration),
    /// An older check made from the same saved index: walk again, but start
    /// from its copy, so differences it already read are not read again and a
    /// check its time limits cut short carries on where it stopped.
    Resume(Arc<SearchIndex>),
    /// Nothing usable: check from the saved index.
    Fresh,
}

impl CheckedOverlays {
    /// What the kept check for `root` can do for a query answered from the
    /// saved index `saved` (generation `generation`) and semantic index
    /// `semantic`. An entry for a different saved index is dropped.
    pub(super) fn lookup(
        &mut self,
        root: &Path,
        generation: &str,
        saved: &Arc<SearchIndex>,
        semantic: Option<&Arc<SemanticIndex>>,
        window: Duration,
    ) -> OverlayLookup {
        let Some(position) = self.entries.iter().position(|entry| entry.root == root) else {
            return OverlayLookup::Fresh;
        };
        let Some(entry) = self.entries.remove(position) else {
            return OverlayLookup::Fresh;
        };
        let age = entry.checked_at.elapsed();
        let same_saved = entry.generation == generation
            && entry
                .saved
                .upgrade()
                .is_some_and(|kept| Arc::ptr_eq(&kept, saved));
        if !same_saved {
            return OverlayLookup::Fresh;
        }
        // The semantic index only feeds a count the next walk recomputes, so
        // a different one rules out reusing the check, not resuming from it.
        let same_semantic = match (&entry.semantic, semantic) {
            (None, None) => true,
            (Some(kept), Some(current)) => kept
                .upgrade()
                .is_some_and(|kept| Arc::ptr_eq(&kept, current)),
            _ => false,
        };
        if age >= window || !same_semantic {
            return OverlayLookup::Resume(entry.checked.index);
        }
        let reused = entry.checked.clone();
        self.entries.push_back(entry);
        OverlayLookup::Reuse(reused, age)
    }

    pub(super) fn remember(
        &mut self,
        root: &Path,
        generation: &str,
        saved: &Arc<SearchIndex>,
        semantic: Option<&Arc<SemanticIndex>>,
        checked: CheckedIndex,
    ) {
        self.forget_root(root);
        self.entries.push_back(CheckedOverlay {
            root: root.to_path_buf(),
            generation: generation.to_string(),
            saved: Arc::downgrade(saved),
            semantic: semantic.map(Arc::downgrade),
            checked,
            checked_at: Instant::now(),
        });
        while self.entries.len() > MAX_CHECKED_ROOTS {
            self.entries.pop_front();
        }
    }

    pub(crate) fn forget_root(&mut self, root: &Path) {
        self.entries.retain(|entry| entry.root != root);
    }

    /// Keep only the projects for which `keep` holds.
    pub(crate) fn retain_roots(&mut self, keep: impl Fn(&Path) -> bool) {
        self.entries.retain(|entry| keep(&entry.root));
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Changed files whose size still matches are hashed this many at a time, so
/// the re-read deadline is checked between batches.
const VERIFY_BATCH: usize = 512;

#[cfg(test)]
thread_local! {
    /// Most hashed or read files one check may handle, standing in for the
    /// re-read deadline in tests that need a pass to stop at a known point.
    static WORK_LIMIT_FOR_TEST: Cell<Option<usize>> = const { Cell::new(None) };
    static REINDEXES_FOR_TEST: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn with_work_limit_for_test<R>(limit: usize, run: impl FnOnce() -> R) -> R {
    WORK_LIMIT_FOR_TEST.with(|slot| {
        let previous = slot.replace(Some(limit));
        let result = run();
        slot.set(previous);
        result
    })
}

/// How many files [`check_against_disk`] has re-indexed on this thread.
#[cfg(test)]
pub(crate) fn reindexes_for_test() -> usize {
    REINDEXES_FOR_TEST.with(Cell::get)
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
    /// Its size or modification time differs from the copy being checked.
    differs: bool,
    /// The difference was resolved: hashed and found unchanged, or read.
    resolved: bool,
}

/// Compare the files under `root` with the saved index `saved` and bring a
/// copy of it in line with them.
///
/// `previous`, when given, is the copy an earlier check of this same saved
/// index produced. The walk then compares the disk with that copy, so only
/// what changed since, or what the earlier check had no time for, is hashed
/// or read. The counts in the result always compare the final copy with
/// `saved`, however many passes built it.
///
/// A file whose size still matches but whose modification time differs (every
/// file of a fresh checkout or worktree) is hashed first and compared with
/// the content hash the index recorded; only a real content difference is
/// re-indexed. Hashing and reading share the re-read deadline.
///
/// `semantic`, when given, is the project's saved semantic index; files it
/// describes with older content are counted in
/// [`DiskCheck::semantic_outdated`] (vectors cannot be recomputed here).
pub(super) fn check_against_disk(
    saved: &Arc<SearchIndex>,
    previous: Option<&Arc<SearchIndex>>,
    root: &Path,
    semantic: Option<&SemanticIndex>,
) -> CheckedIndex {
    let budgets = budgets();
    #[cfg(test)]
    WALKS_FOR_TEST.with(|walks| walks.set(walks.get() + 1));
    let base = previous.unwrap_or(saved);
    let base_is_saved = Arc::ptr_eq(base, saved);
    let walk_started = Instant::now();
    let walk_deadline = walk_started + budgets.walk;
    let stop_requested =
        |deadline: Instant| Instant::now() >= deadline || crate::executor::current_job_cancelled();

    let mut check = DiskCheck::default();
    let mut walked: Vec<WalkedFile> = Vec::new();
    let mut seen_base = vec![false; base.files.len()];
    let mut seen_saved = if base_is_saved {
        Vec::new()
    } else {
        vec![false; saved.files.len()]
    };
    let mut walk_complete = true;
    // `SearchIndex::build` enumerates files with this same walker
    // (`project_walk_builder`, which honours .gitignore and .aftignore and
    // skips build directories), so a path this walk skips is one the index
    // never held either, and a file the finished walk does not reach is gone.
    for entry in crate::search_index::project_walk_builder(root).build() {
        if stop_requested(walk_deadline) {
            walk_complete = false;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let size = metadata.len();
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let path = entry.path();
        let in_base = base.path_to_id.get(path).copied();
        if let Some(slot) = in_base.and_then(|file_id| seen_base.get_mut(file_id as usize)) {
            *slot = true;
        }
        let in_saved = if base_is_saved {
            in_base.is_some()
        } else {
            let file_id = saved.path_to_id.get(path).copied();
            if let Some(slot) = file_id.and_then(|file_id| seen_saved.get_mut(file_id as usize)) {
                *slot = true;
            }
            file_id.is_some()
        };
        let matches_base = in_base
            .and_then(|file_id| base.files.get(file_id as usize))
            .is_some_and(|recorded| recorded.size == size && recorded.modified == modified);
        if let Some(semantic) = semantic {
            let outdated = match semantic.recorded_stat(path) {
                Some((recorded_mtime, recorded_size)) => {
                    recorded_mtime != modified
                        || recorded_size.is_some_and(|recorded| recorded != size)
                }
                None => !in_saved,
            };
            if outdated {
                check.semantic_outdated += 1;
            }
        }
        walked.push(WalkedFile {
            path: path.to_path_buf(),
            size,
            modified,
            differs: !matches_base,
            resolved: false,
        });
    }
    check.files_examined = walked.len();
    check.walk_complete = walk_complete;

    let unseen = |index: &SearchIndex, seen: &[bool]| -> Vec<PathBuf> {
        index
            .files
            .iter()
            .zip(seen)
            .filter(|(entry, seen)| !**seen && !entry.path.as_os_str().is_empty())
            .map(|(entry, _)| entry.path.clone())
            .collect()
    };
    let (removed_from_base, removed_from_saved) = if !walk_complete {
        (Vec::new(), Vec::new())
    } else if base_is_saved {
        let removed = unseen(base, &seen_base);
        (removed.clone(), removed)
    } else {
        (unseen(base, &seen_base), unseen(saved, &seen_saved))
    };
    let walk_time = walk_started.elapsed();

    let mut work: Vec<usize> = (0..walked.len()).filter(|&i| walked[i].differs).collect();
    let reread_started = Instant::now();
    let mut copy = None;
    if !work.is_empty() || !removed_from_base.is_empty() {
        let mut overlay = SearchIndex::clone(base);
        for path in &removed_from_base {
            overlay.remove_file(path);
        }
        resolve_differences(&mut overlay, &mut walked, &mut work, budgets.reread);
        copy = Some(overlay);
    }
    let reread_time = reread_started.elapsed();

    // Describe the final copy against the saved index.
    let current: &SearchIndex = copy.as_ref().unwrap_or(base.as_ref());
    let mut digest_entries: Vec<(&Path, u64, SystemTime)> = Vec::new();
    for file in &walked {
        let recorded = saved
            .path_to_id
            .get(&file.path)
            .and_then(|&file_id| saved.files.get(file_id as usize));
        if file.differs && !file.resolved {
            check.not_reread += 1;
            if recorded.is_none() {
                check.added += 1;
            }
            continue;
        }
        let held = current
            .path_to_id
            .get(&file.path)
            .and_then(|&file_id| current.files.get(file_id as usize));
        let Some(held) = held else {
            // Gone between the walk and the read.
            continue;
        };
        match recorded {
            None => {
                check.added += 1;
                check.reread += 1;
                digest_entries.push((&file.path, file.size, file.modified));
            }
            Some(recorded) if recorded.size == file.size && recorded.modified == file.modified => {}
            Some(recorded) if recorded.content_hash == held.content_hash => {
                check.content_unchanged += 1;
            }
            Some(_) => {
                check.changed += 1;
                check.reread += 1;
                digest_entries.push((&file.path, file.size, file.modified));
            }
        }
    }
    check.removed = removed_from_saved.len();

    let applied_digest =
        (!digest_entries.is_empty() || !removed_from_saved.is_empty()).then(|| {
            digest_entries.sort();
            let mut removed: Vec<&PathBuf> = removed_from_saved.iter().collect();
            removed.sort();
            let mut digest = blake3::Hasher::new();
            for path in removed {
                digest.update(b"-");
                digest.update(path.as_os_str().as_encoded_bytes());
            }
            for (path, size, modified) in digest_entries {
                digest.update(b"+");
                digest.update(path.as_os_str().as_encoded_bytes());
                digest.update(&size.to_le_bytes());
                let since_epoch = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
                digest.update(&since_epoch.as_nanos().to_le_bytes());
            }
            digest.finalize().to_hex()[..16].to_string()
        });

    CheckedIndex {
        index: copy.map(Arc::new).unwrap_or_else(|| Arc::clone(base)),
        check,
        applied_digest,
        walk_time,
        reread_time,
    }
}

/// Hash or read the walked files listed in `work` into `copy`, program source
/// first, until `budget` runs out. Files whose size matches the copy's record
/// are hashed in parallel batches and only re-indexed when their bytes
/// differ; a hash match just records the new modification time, so the next
/// check of this copy sees the file as unchanged.
fn resolve_differences(
    copy: &mut SearchIndex,
    walked: &mut [WalkedFile],
    work: &mut [usize],
    budget: Duration,
) {
    use crate::cache_freshness::{FileFreshness, FreshnessVerdict, VerifyStrategy};

    // Read program source before documentation and data files (the
    // `classify_file` order), so if the deadline stops the re-read, the files
    // a code search most needs current are the ones already read.
    work.sort_by_cached_key(|&i| (classify_file(&walked[i].path), walked[i].path.clone()));
    let deadline = Instant::now() + budget;
    let stop_requested = || Instant::now() >= deadline || crate::executor::current_job_cancelled();
    let mut remaining = work_limit();
    for batch in work.chunks(VERIFY_BATCH) {
        if stop_requested() || remaining == 0 {
            return;
        }
        let batch = &batch[..batch.len().min(remaining)];
        remaining -= batch.len();
        let mut to_hash = Vec::new();
        let mut to_read = Vec::new();
        for &i in batch {
            let file = &walked[i];
            let recorded = copy
                .path_to_id
                .get(&file.path)
                .and_then(|&file_id| copy.files.get(file_id as usize));
            match recorded {
                Some(recorded) if recorded.size == file.size => to_hash.push((
                    i,
                    file.path.clone(),
                    FileFreshness {
                        mtime: recorded.modified,
                        size: recorded.size,
                        content_hash: recorded.content_hash,
                    },
                )),
                _ => to_read.push(i),
            }
        }
        for (i, path, verdict) in
            crate::cache_freshness::verify_files_bounded(to_hash, VerifyStrategy::StatFirst)
        {
            match verdict {
                FreshnessVerdict::HotFresh => walked[i].resolved = true,
                FreshnessVerdict::ContentFresh {
                    new_mtime,
                    new_size,
                } => {
                    if let Some(&file_id) = copy.path_to_id.get(&path) {
                        if let Some(entry) =
                            Arc::make_mut(&mut copy.files).get_mut(file_id as usize)
                        {
                            entry.modified = new_mtime;
                            entry.size = new_size;
                        }
                    }
                    walked[i].resolved = true;
                }
                FreshnessVerdict::Stale | FreshnessVerdict::Deleted => to_read.push(i),
            }
        }
        for i in to_read {
            if stop_requested() {
                return;
            }
            #[cfg(test)]
            REINDEXES_FOR_TEST.with(|count| count.set(count.get() + 1));
            copy.update_file(&walked[i].path);
            walked[i].resolved = true;
        }
    }
}

fn files(count: usize) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
}

fn was_were(count: usize) -> &'static str {
    if count == 1 {
        "was"
    } else {
        "were"
    }
}

/// ` 6 s ago` for a check reused from an earlier query, empty for one made
/// for this query.
pub(super) fn checked_ago(age: Option<Duration>) -> String {
    age.map(|age| format!(" {} ago", super::format_coarse_age(age)))
        .unwrap_or_default()
}

/// The sentence a reply ends with when `check` covered every file. `age` is
/// how long ago the check was made when an earlier query's check is reused.
pub(super) fn verified_line(check: &DiskCheck, root: &Path, age: Option<Duration>) -> String {
    let mut differences = Vec::new();
    if check.changed > 0 {
        differences.push(format!("{} changed", files(check.changed)));
    }
    if check.added > 0 {
        differences.push(format!(
            "{} {} added",
            files(check.added),
            was_were(check.added)
        ));
    }
    if check.removed > 0 {
        differences.push(format!(
            "{} {} deleted",
            files(check.removed),
            was_were(check.removed)
        ));
    }
    let head = format!(
        "Checked the saved AFT index of {} against all {} on disk{}",
        root.display(),
        files(check.files_examined),
        checked_ago(age)
    );
    if differences.is_empty() {
        format!("{head}; none changed since it was saved.")
    } else {
        format!(
            "{head}; since it was saved {}, and the current files were searched.",
            differences.join(", ")
        )
    }
}

/// What `check` left unverified, for a reply that could not cover every file.
pub(super) fn unverified_detail(check: &DiskCheck) -> String {
    let read = if check.reread > 0 {
        format!(
            ", and {} that changed or {} added among them {} read from disk",
            files(check.reread),
            was_were(check.reread),
            was_were(check.reread)
        )
    } else {
        String::new()
    };
    if check.walk_complete {
        format!(
            "all {} on disk were compared with it{read}, but {} that changed or {} added could not be read before the time limit",
            files(check.files_examined),
            files(check.not_reread),
            was_were(check.not_reread)
        )
    } else {
        let unread = if check.not_reread > 0 {
            format!(
                "; {} more that changed or {} added could not be read in time",
                files(check.not_reread),
                was_were(check.not_reread)
            )
        } else {
            String::new()
        };
        format!(
            "only {} on disk were compared with it before the time limit{read}{unread}",
            files(check.files_examined)
        )
    }
}

/// The sentence added when the semantic lane answered from a saved semantic
/// index that predates some of the files.
pub(super) fn semantic_outdated_line(check: &DiskCheck, root: &Path) -> Option<String> {
    (check.semantic_outdated > 0).then(|| {
        format!(
            "The saved semantic index of {} predates {} that changed or {} added since: their ranking by meaning reflects older content or is missing, though their current text was searched.",
            root.display(),
            files(check.semantic_outdated),
            was_were(check.semantic_outdated)
        )
    })
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

    fn postings_find(index: &SearchIndex, root: &Path, needle: &str) -> Vec<PathBuf> {
        let compiled = match crate::pattern_compile::compile(
            needle,
            crate::pattern_compile::CompileOpts {
                literal: true,
                ..crate::pattern_compile::CompileOpts::default()
            },
        ) {
            crate::pattern_compile::CompileResult::Ok(compiled) => compiled,
            other => panic!("literal compile failed: {other:?}"),
        };
        let mut files: Vec<PathBuf> = index
            .snapshot()
            .search_grep_bounded(
                &compiled,
                &[],
                &[],
                root,
                100,
                None,
                1_000,
                Duration::from_secs(5),
            )
            .unwrap()
            .matches
            .into_iter()
            .map(|grep_match| grep_match.file)
            .collect();
        files.sort();
        files.dedup();
        files
    }

    fn backdate(path: &Path) {
        // Equal sizes with different mtimes must still count as a change, so
        // give the original file an mtime the rewrite cannot share.
        let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(path, old).expect("backdate file");
    }

    #[test]
    fn unchanged_project_is_verified_and_keeps_the_saved_index() {
        let (_dir, root) = project(&[("src/lib.rs", "pub fn kept_needle() {}\n")]);
        let index = Arc::new(SearchIndex::build(&root));

        let checked = check_against_disk(&index, None, &root, None);

        assert!(checked.check.verified(), "{:?}", checked.check);
        assert_eq!(checked.check.files_examined, 1);
        assert_eq!(checked.check.changed + checked.check.added, 0);
        assert!(Arc::ptr_eq(&checked.index, &index));
        assert!(checked.applied_digest.is_none());
    }

    #[test]
    fn added_changed_and_deleted_files_are_answered_from_disk() {
        let (_dir, root) = project(&[
            ("src/edited.rs", "pub fn before_edit_needle() {}\n"),
            ("src/deleted.rs", "pub fn deleted_needle() {}\n"),
            ("src/same.rs", "pub fn same_needle() {}\n"),
        ]);
        backdate(&root.join("src/edited.rs"));
        let index = Arc::new(SearchIndex::build(&root));
        std::fs::write(
            root.join("src/edited.rs"),
            "pub fn after_edit_needle() {}\n",
        )
        .unwrap();
        std::fs::remove_file(root.join("src/deleted.rs")).unwrap();
        std::fs::write(root.join("src/added.rs"), "pub fn added_needle() {}\n").unwrap();
        assert!(postings_find(&index, &root, "added_needle").is_empty());

        let checked = check_against_disk(&index, None, &root, None);

        assert_eq!(
            checked.check,
            DiskCheck {
                files_examined: 3,
                walk_complete: true,
                changed: 1,
                content_unchanged: 0,
                added: 1,
                removed: 1,
                reread: 2,
                not_reread: 0,
                semantic_outdated: 0,
            }
        );
        assert!(checked.check.verified());
        assert_eq!(
            postings_find(&checked.index, &root, "added_needle"),
            vec![root.join("src/added.rs")]
        );
        assert_eq!(
            postings_find(&checked.index, &root, "after_edit_needle"),
            vec![root.join("src/edited.rs")]
        );
        assert!(postings_find(&checked.index, &root, "deleted_needle").is_empty());
        assert_eq!(
            postings_find(&checked.index, &root, "same_needle"),
            vec![root.join("src/same.rs")]
        );
        // The saved index itself is left exactly as it was loaded.
        assert!(postings_find(&index, &root, "added_needle").is_empty());
        let again = check_against_disk(&index, None, &root, None);
        assert_eq!(again.applied_digest, checked.applied_digest);
    }

    fn borrowed_search_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        tempfile::TempDir,
        tempfile::TempDir,
        crate::context::AppContext,
    ) {
        let (dir, root) = project(&[(
            "src/lib.rs",
            "// Assemble the zephyrine quokkalith.\npub fn assemble() {}\n",
        )]);
        let mut git = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(git.current_dir(&root));
        assert!(git
            .args(["init", "-q"])
            .status()
            .expect("git init")
            .success());
        let storage = tempfile::tempdir().expect("storage");
        let cache_dir = crate::search_index::resolve_cache_dir(&root, Some(storage.path()));
        SearchIndex::build(&root).write_to_disk(&cache_dir, None);
        let session = tempfile::tempdir().expect("session");
        let ctx = crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(session.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        (dir, root, storage, session, ctx)
    }

    fn prose_search(ctx: &crate::context::AppContext, root: &Path) -> serde_json::Value {
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "disk-check-reuse",
            "command": "semantic_search",
            "query": "where is the zephyrine quokkalith assembled",
            "top_k": 5,
            "path": root.display().to_string(),
        }))
        .expect("request");
        serde_json::to_value(super::super::handle_semantic_search(&request, ctx)).expect("response")
    }

    /// Queries a few seconds apart reuse one check instead of walking the
    /// project each time, and say how old the check is; once the window has
    /// passed the project is walked again.
    #[test]
    fn queries_within_the_reuse_window_walk_the_project_once() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_dir, root, _storage, _session, ctx) = borrowed_search_fixture();
        let long_window = Budgets {
            walk: Duration::from_secs(5),
            reread: Duration::from_secs(5),
            reuse_window: Duration::from_secs(600),
        };

        let (first, second, walks) = with_all_budgets_for_test(long_window, || {
            let before = walks_for_test();
            let first = prose_search(&ctx, &root);
            let second = prose_search(&ctx, &root);
            (first, second, walks_for_test() - before)
        });

        assert_eq!(walks, 1, "two queries inside the window walk once");
        assert_eq!(first["saved_index_check"]["reused"], false, "{first:#?}");
        assert_eq!(second["saved_index_check"]["reused"], true, "{second:#?}");
        let first_text = first["text"].as_str().expect("text");
        let second_text = second["text"].as_str().expect("text");
        assert!(
            first_text.contains("against all 1 file on disk; none changed"),
            "{first_text}"
        );
        assert!(
            second_text.contains("against all 1 file on disk 0 s ago; none changed"),
            "{second_text}"
        );
        assert_eq!(ctx.with_checked_overlays(|overlays| overlays.len()), 1);

        let expired = Budgets {
            reuse_window: Duration::ZERO,
            ..long_window
        };
        let (third, walks) = with_all_budgets_for_test(expired, || {
            let before = walks_for_test();
            let third = prose_search(&ctx, &root);
            (third, walks_for_test() - before)
        });
        assert_eq!(walks, 1, "a query after the window walks again");
        assert_eq!(third["saved_index_check"]["reused"], false, "{third:#?}");

        assert!(ctx.evict_idle_artifacts());
        assert_eq!(
            ctx.with_checked_overlays(|overlays| overlays.len()),
            0,
            "dropping the borrowed index drops its checked copy"
        );
    }

    /// A fresh checkout or worktree has every file's modification time
    /// changed and its bytes the same. Those files are hashed and kept on the
    /// saved postings; only a real content change is re-indexed.
    #[test]
    fn mtime_only_changes_are_hashed_not_reindexed() {
        let (_dir, root) = project(&[
            ("src/a.rs", "pub fn a_needle() {}\n"),
            ("src/b.rs", "pub fn b_needle() {}\n"),
            ("src/c.rs", "pub fn c_needle() {}\n"),
        ]);
        for name in ["a", "b", "c"] {
            backdate(&root.join(format!("src/{name}.rs")));
        }
        let index = Arc::new(SearchIndex::build(&root));
        let touched = filetime::FileTime::from_unix_time(1_100_000_000, 0);
        for name in ["a", "b"] {
            filetime::set_file_mtime(root.join(format!("src/{name}.rs")), touched).unwrap();
        }
        // Same length, different bytes: only a hash can tell this one changed.
        std::fs::write(root.join("src/c.rs"), "pub fn x_needle() {}\n").unwrap();

        let before = reindexes_for_test();
        let checked = check_against_disk(&index, None, &root, None);

        assert_eq!(reindexes_for_test() - before, 1, "only c.rs is re-indexed");
        assert!(checked.check.verified(), "{:?}", checked.check);
        assert_eq!(checked.check.content_unchanged, 2);
        assert_eq!(checked.check.changed, 1);
        assert_eq!(checked.check.reread, 1);
        assert_eq!(
            postings_find(&checked.index, &root, "x_needle"),
            vec![root.join("src/c.rs")]
        );
        assert_eq!(
            postings_find(&checked.index, &root, "a_needle"),
            vec![root.join("src/a.rs")]
        );

        // Checking that checked copy again finds nothing left to hash or
        // read, and still describes the copy against the saved index.
        let before = reindexes_for_test();
        let again = check_against_disk(&index, Some(&checked.index), &root, None);
        assert_eq!(reindexes_for_test() - before, 0);
        assert_eq!(again.check, checked.check);
        assert_eq!(again.applied_digest, checked.applied_digest);
    }

    /// A check cut short by its time limit resumes, once the reuse window has
    /// passed, from the in-memory copy of the index it left; the reply then
    /// counts every file that copy holds, not only the files the second pass
    /// processed.
    #[test]
    fn a_capped_check_completes_over_two_windows() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_dir, root, _storage, _session, ctx) = borrowed_search_fixture();
        for file in 0..4 {
            std::fs::write(
                root.join(format!("src/added_{file}.rs")),
                format!("pub fn added_after_save_{file}() {{}}\n"),
            )
            .unwrap();
        }

        // The work limit is what cuts each pass short here, so the time
        // limits stay out of the way: the check's own budgets are wide, and the
        // first search waits for the borrowed index to finish loading on its
        // background thread. Without that wait a slow runner answers the first
        // search from a degraded empty index, which never runs the disk check.
        let pass = || {
            super::super::with_first_search_index_load_wait_budget_for_test(
                Duration::from_secs(60),
                || {
                    with_budgets_for_test(Duration::from_secs(60), Duration::from_secs(60), || {
                        with_work_limit_for_test(2, || {
                            let before = reindexes_for_test();
                            let response = prose_search(&ctx, &root);
                            (response, reindexes_for_test() - before)
                        })
                    })
                },
            )
        };
        let (first, first_reads) = pass();
        let (second, second_reads) = pass();

        assert_eq!(first_reads, 2, "{first:#?}");
        assert_eq!(first["complete"], false, "{first:#?}");
        assert_eq!(first["saved_index_check"]["added"], 4);
        assert_eq!(first["saved_index_check"]["reread"], 2);
        assert_eq!(first["saved_index_check"]["not_reread"], 2);

        assert_eq!(
            second_reads, 2,
            "the second pass reads only what the first left: {second:#?}"
        );
        assert_eq!(second["saved_index_check"]["complete"], true, "{second:#?}");
        assert_eq!(second["saved_index_check"]["reused"], false);
        assert_eq!(second["saved_index_check"]["added"], 4);
        assert_eq!(second["saved_index_check"]["reread"], 4);
        assert_eq!(second["saved_index_check"]["not_reread"], 0);
        let text = second["text"].as_str().expect("text");
        assert!(
            text.contains("against all 5 files on disk; since it was saved 4 files were added"),
            "{text}"
        );
    }

    #[test]
    fn checked_overlays_keep_one_entry_per_root_and_at_most_eight_roots() {
        let index = Arc::new(SearchIndex::new());
        let checked = CheckedIndex {
            index: Arc::clone(&index),
            check: DiskCheck::default(),
            applied_digest: None,
            walk_time: Duration::ZERO,
            reread_time: Duration::ZERO,
        };
        let mut overlays = CheckedOverlays::default();
        for round in 0..2 {
            for root in 0..10 {
                overlays.remember(
                    Path::new(&format!("/root{root}")),
                    &format!("generation{round}"),
                    &index,
                    None,
                    checked.clone(),
                );
            }
        }
        assert_eq!(overlays.len(), MAX_CHECKED_ROOTS);
        let window = Duration::from_secs(600);
        let is_fresh = |lookup: OverlayLookup| matches!(lookup, OverlayLookup::Fresh);
        assert!(
            is_fresh(overlays.lookup(Path::new("/root1"), "generation1", &index, None, window)),
            "the least recently used roots were dropped"
        );
        assert!(
            is_fresh(overlays.lookup(Path::new("/root9"), "generation0", &index, None, window)),
            "a check made for an older generation is neither reused nor resumed"
        );
        assert!(matches!(
            overlays.lookup(Path::new("/root8"), "generation1", &index, None, window),
            OverlayLookup::Reuse(..)
        ));
        assert!(
            matches!(
                overlays.lookup(
                    Path::new("/root6"),
                    "generation1",
                    &index,
                    None,
                    Duration::ZERO
                ),
                OverlayLookup::Resume(_)
            ),
            "an expired check of the same saved index is resumed"
        );
        let other = Arc::new(SearchIndex::new());
        assert!(
            is_fresh(overlays.lookup(Path::new("/root7"), "generation1", &other, None, window)),
            "a check made from a different saved index is neither reused nor resumed"
        );
    }

    #[test]
    fn a_walk_stopped_by_its_deadline_is_not_verified_and_drops_nothing() {
        let (_dir, root) = project(&[
            ("src/a.rs", "pub fn a_needle() {}\n"),
            ("src/b.rs", "pub fn b_needle() {}\n"),
        ]);
        let index = Arc::new(SearchIndex::build(&root));
        std::fs::write(root.join("src/c.rs"), "pub fn c_needle() {}\n").unwrap();

        let checked = with_budgets_for_test(Duration::ZERO, Duration::from_secs(5), || {
            check_against_disk(&index, None, &root, None)
        });

        assert!(!checked.check.walk_complete);
        assert!(!checked.check.verified());
        assert_eq!(checked.check.files_examined, 0);
        assert_eq!(checked.check.removed, 0, "unvisited files are not deleted");
        assert!(Arc::ptr_eq(&checked.index, &index));
        assert!(unverified_detail(&checked.check).starts_with("only 0 files on disk were compared"));
    }

    #[test]
    fn differences_left_unread_by_the_deadline_are_not_verified() {
        let (_dir, root) = project(&[("src/a.rs", "pub fn a_needle() {}\n")]);
        let index = Arc::new(SearchIndex::build(&root));
        std::fs::write(root.join("src/new.rs"), "pub fn new_needle() {}\n").unwrap();

        let checked = with_budgets_for_test(Duration::from_secs(5), Duration::ZERO, || {
            check_against_disk(&index, None, &root, None)
        });

        assert!(checked.check.walk_complete);
        assert_eq!(checked.check.added, 1);
        assert_eq!(checked.check.not_reread, 1);
        assert!(!checked.check.verified());
        assert!(postings_find(&checked.index, &root, "new_needle").is_empty());
        assert!(
            unverified_detail(&checked.check).ends_with(
                "but 1 file that changed or was added could not be read before the time limit"
            ),
            "{}",
            unverified_detail(&checked.check)
        );
    }

    /// Prints what the check costs on real trees. Nothing is written: a saved
    /// index is only read from `AFT_DISK_CHECK_PROBE_STORAGE` when the tree has
    /// one, otherwise one is built in memory.
    #[test]
    #[ignore = "measures the disk check on the trees named in AFT_DISK_CHECK_PROBE_ROOTS"]
    fn disk_check_cost_probe() {
        let roots = std::env::var("AFT_DISK_CHECK_PROBE_ROOTS").expect("roots");
        let storage = std::env::var_os("AFT_DISK_CHECK_PROBE_STORAGE").map(PathBuf::from);
        let uncapped = Budgets {
            walk: Duration::from_secs(600),
            reread: Duration::from_secs(600),
            reuse_window: Duration::ZERO,
        };
        for root in roots.split(':') {
            let root = std::fs::canonicalize(root).expect("root");
            // With an empty index every file is new and nothing is re-read:
            // this is the first walk and stat of the tree in this process.
            let first = with_all_budgets_for_test(
                Budgets {
                    reread: Duration::ZERO,
                    ..uncapped
                },
                || check_against_disk(&Arc::new(SearchIndex::new()), None, &root, None),
            );
            eprintln!(
                "PROBE {} first_walk_ms={} stats={}",
                root.display(),
                first.walk_time.as_millis(),
                first.check.files_examined
            );
            let saved = storage.as_ref().and_then(|storage| {
                let key = crate::search_index::artifact_cache_key(&root);
                let cache_dir =
                    crate::search_index::resolve_cache_dir_with_key(&key, Some(storage));
                match crate::readonly_artifacts::open_search_index_cancellable(
                    &root,
                    cache_dir,
                    10_000_000,
                    Duration::from_secs(120),
                    &|| true,
                ) {
                    crate::readonly_artifacts::ReadOnlyArtifact::Fresh(index) => Some(index),
                    crate::readonly_artifacts::ReadOnlyArtifact::Stale(stale) => Some(stale.index),
                    _ => None,
                }
            });
            let (index, source) = match saved {
                Some(index) => (Arc::new(index), "saved"),
                None => (Arc::new(SearchIndex::build(&root)), "built-now"),
            };
            // Run three fresh capped checks, then capped checks that each
            // resume from the in-memory copy left by the preceding check (as
            // successive reuse windows do), then one uncapped check that
            // starts from the saved index.
            let mut previous: Option<Arc<SearchIndex>> = None;
            for (label, resume, budgets) in [
                ("capped", false, None),
                ("capped", false, None),
                ("capped", false, None),
                ("resumed-1", true, None),
                ("resumed-2", true, None),
                ("resumed-3", true, None),
                ("uncapped", false, Some(uncapped)),
            ] {
                let base = if resume { previous.clone() } else { None };
                let run = || check_against_disk(&index, base.as_ref(), &root, None);
                let checked = match budgets {
                    Some(budgets) => with_all_budgets_for_test(budgets, run),
                    None => run(),
                };
                previous = Some(Arc::clone(&checked.index));
                let check = &checked.check;
                eprintln!(
                    "PROBE {} index={source} indexed={} {label} walk_ms={} stats={} walk_complete={} changed={} content_unchanged={} added={} removed={} reread={} not_reread={} reread_ms={}",
                    root.display(),
                    index.file_count(),
                    checked.walk_time.as_millis(),
                    check.files_examined,
                    check.walk_complete,
                    check.changed,
                    check.content_unchanged,
                    check.added,
                    check.removed,
                    check.reread,
                    check.not_reread,
                    checked.reread_time.as_millis()
                );
            }
        }
    }

    #[test]
    fn verified_line_names_each_kind_of_difference() {
        let root = Path::new("/p");
        let mut check = DiskCheck {
            files_examined: 12,
            walk_complete: true,
            ..DiskCheck::default()
        };
        assert_eq!(
            verified_line(&check, root, None),
            "Checked the saved AFT index of /p against all 12 files on disk; none changed since it was saved."
        );
        assert_eq!(
            verified_line(&check, root, Some(Duration::from_secs(6))),
            "Checked the saved AFT index of /p against all 12 files on disk 6 s ago; none changed since it was saved."
        );
        check.changed = 2;
        check.added = 1;
        check.removed = 3;
        assert_eq!(
            verified_line(&check, root, None),
            "Checked the saved AFT index of /p against all 12 files on disk; since it was saved 2 files changed, 1 file was added, 3 files were deleted, and the current files were searched."
        );
    }
}
