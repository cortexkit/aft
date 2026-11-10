//! grep and glob, read from the data layer.
//!
//! When the process runs the data layer and the root's view has a ready text
//! plane, grep and glob list the checkout's files from the view instead of
//! from today's trigram index. Only where the files come from changes: grep
//! takes its candidates from [`CheckoutView::text_candidates`] and glob lists
//! the view's paths, and both then run today's indexed search over those
//! files. They do it by projecting the files into a [`SearchIndexSnapshot`]
//! that holds no postings, so the snapshot treats every file as unindexed
//! and reads, matches, filters, orders and bounds them exactly as an
//! answer from today's index does.
//!
//! A path changed since the publication is listed as it is on disk now:
//! with its current modification time when it is a file, and not at all
//! when the change was a deletion or the path is now a symlink or a folder.
//! A path that went from disk with no change recorded for it stays listed,
//! so a reply that reads it says it was not on disk, as today's replies do
//! for an index that is out of date.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aft_store::checkout::{PathKind, PathRecord};
use aft_store::text::{plan_pattern, Query, Scope};

use crate::context::AppContext;
use crate::data_layer::checkout::{CheckoutView, DataError, PlaneStatus};
use crate::pattern_compile::CompiledPattern;
use crate::search_index::{FileEntry, PathFilters, SearchIndexSnapshot, DEFAULT_MAX_FILE_SIZE};

#[cfg(test)]
mod tests;

/// The data layer's view of this root, when grep and glob may answer from
/// it: the process runs the data layer, the checkout has published, and its
/// text plane covers every path.
pub(crate) fn ready_view(ctx: &AppContext) -> Option<CheckoutView> {
    #[cfg(test)]
    if test_hooks::today_only() {
        return None;
    }
    let view = ctx.data_view()?;
    (view.status().text == PlaneStatus::Ready).then_some(view)
}

/// One search root inside a view's checkout, and the checkout's files under
/// it as they are now.
pub(crate) struct ViewScope {
    view: CheckoutView,
    /// The root as a checkout path, `/`-separated; empty for the whole
    /// checkout.
    relative: Vec<u8>,
    /// Each path in scope changed since the publication, as it is on disk
    /// now.
    changed: BTreeMap<Vec<u8>, OnDisk>,
}

/// A changed path as it is on disk now.
#[derive(Clone, Copy, Debug)]
enum OnDisk {
    /// A regular file, with its size and modification time.
    File(u64, SystemTime),
    /// No file, though the change recorded one: it went after the change
    /// was recorded. It stays listed, so a reply that reads it says it was
    /// not on disk, as today's replies do for a file their index lists.
    Stale,
    /// Never listed: the change recorded a deletion, or the path is now a
    /// symlink or a folder, which today's index never lists as a file.
    Absent,
}

impl ViewScope {
    /// `search_root` as a scope of `view`, or `None` when it is outside the
    /// view's checkout.
    pub(crate) fn of(view: CheckoutView, search_root: &Path) -> Option<Self> {
        let relative = checkout_path(view.root(), search_root)?;
        let root = view.root().to_path_buf();
        let changed = view
            .changed_paths()
            .into_iter()
            .filter(|path| in_scope(&relative, path))
            .map(|path| {
                let now = match absolute(&root, &path).map(std::fs::symlink_metadata) {
                    Some(Ok(metadata)) if metadata.is_file() => {
                        OnDisk::File(metadata.len(), metadata.modified().unwrap_or(UNIX_EPOCH))
                    }
                    Some(Ok(_)) => OnDisk::Absent,
                    // The text plane holds nothing for a recorded deletion.
                    _ => match view.path_text(&path) {
                        Ok(None) => OnDisk::Absent,
                        _ => OnDisk::Stale,
                    },
                };
                (path, now)
            })
            .collect();
        #[cfg(test)]
        test_hooks::note_answer();
        Some(Self {
            view,
            relative,
            changed,
        })
    }

    /// Whether the scope is the whole checkout, which always answers from
    /// the view, even when it holds no file.
    pub(crate) fn is_whole_checkout(&self) -> bool {
        self.relative.is_empty()
    }

    /// Whether any file of the checkout is in scope now.
    pub(crate) fn has_files(&self) -> bool {
        self.files().next().is_some()
    }

    /// Whether any file in scope passes `filters`, which match paths
    /// relative to the checkout root.
    pub(crate) fn has_files_matching(&self, filters: &PathFilters) -> bool {
        let root = self.view.root();
        self.files()
            .filter_map(|(path, _, _)| absolute(root, path))
            .any(|path| filters.matches(root, &path))
    }

    /// grep's candidate files for `pattern`, every file in scope that may
    /// match it, as a snapshot that reads each one.
    ///
    /// # Errors
    ///
    /// When the view's text plane cannot be read.
    pub(crate) fn candidates(
        &self,
        pattern: &CompiledPattern,
    ) -> Result<SearchIndexSnapshot, DataError> {
        // A pattern the planner cannot parse still has candidates: every
        // file in scope, which the search reads and checks.
        let query = plan_pattern(
            &pattern.raw_pattern_for_trigrams(),
            pattern.case_insensitive(),
        )
        .unwrap_or(Query::All);
        let state = self.view.state();
        // A root that names one file searches it alone: the planner is
        // asked about the file's folder, and only the file is kept.
        let names_file = !self.relative.is_empty()
            && (state.position(&self.relative).is_some()
                || matches!(self.changed.get(&self.relative), Some(OnDisk::File(..))));
        let folder = if names_file {
            self.relative
                .iter()
                .rposition(|byte| *byte == b'/')
                .map_or(&[][..], |slash| &self.relative[..slash])
        } else {
            &self.relative
        };
        let scope = if folder.is_empty() {
            Scope::All
        } else {
            Scope::Directory(folder)
        };
        let files = self
            .view
            .text_candidates(&query, scope)?
            .into_iter()
            .filter(|path| in_scope(&self.relative, path))
            .filter_map(|path| {
                let (size, modified) = match self.changed.get(&path) {
                    Some(OnDisk::File(size, modified)) => (*size, *modified),
                    // The read fails and the reply says it was not on disk.
                    Some(OnDisk::Stale) => (0, UNIX_EPOCH),
                    Some(OnDisk::Absent) => return None,
                    None => {
                        let record = state.get(&path)?;
                        (record.stat.size, modified_at(record.stat.mtime_ns))
                    }
                };
                self.entry(&path, size, modified)
            });
        Ok(self.snapshot(files))
    }

    /// Every file in scope, as a snapshot for glob to list.
    pub(crate) fn listing(&self) -> SearchIndexSnapshot {
        self.snapshot(
            self.files()
                .filter_map(|(path, size, modified)| self.entry(path, size, modified)),
        )
    }

    fn snapshot(&self, files: impl Iterator<Item = FileEntry>) -> SearchIndexSnapshot {
        SearchIndexSnapshot::from_verified_rows(
            self.view.root(),
            DEFAULT_MAX_FILE_SIZE,
            files.map(|file| (file, None)),
        )
    }

    fn entry(&self, path: &[u8], size: u64, modified: SystemTime) -> Option<FileEntry> {
        Some(FileEntry {
            path: absolute(self.view.root(), path)?,
            size,
            modified,
            content_hash: crate::cache_freshness::zero_hash(),
        })
    }

    /// The files in scope with their sizes and modification times: the
    /// published files not changed since, then the changed paths that are
    /// files now or went without a recorded change. Symlinks are left out,
    /// as today's index walk never lists one.
    fn files(&self) -> impl Iterator<Item = (&[u8], u64, SystemTime)> {
        let state = self.view.state();
        let range = if self.relative.is_empty() {
            0..state.paths().len()
        } else if let Some(index) = state.position(&self.relative) {
            index..index + 1
        } else {
            state.directory(&self.relative)
        };
        let published = state.paths()[range]
            .iter()
            .filter(|record| record.kind != PathKind::Symlink)
            .filter(|record| !self.changed.contains_key(&record.path))
            .map(|record: &PathRecord| {
                (
                    record.path.as_slice(),
                    record.stat.size,
                    modified_at(record.stat.mtime_ns),
                )
            });
        let changed = self.changed.iter().filter_map(|(path, now)| match now {
            OnDisk::File(size, modified) => Some((path.as_slice(), *size, *modified)),
            OnDisk::Stale => Some((path.as_slice(), 0, UNIX_EPOCH)),
            OnDisk::Absent => None,
        });
        published.chain(changed)
    }
}

/// Whether checkout path `path` is `scope` itself or inside it.
fn in_scope(scope: &[u8], path: &[u8]) -> bool {
    scope.is_empty()
        || path == scope
        || (path.len() > scope.len() && path.starts_with(scope) && path[scope.len()] == b'/')
}

/// `search_root` as a `/`-separated path of the checkout at `root`, or
/// `None` when it is outside it. The root is compared in its canonical
/// form, as today's index compares a search root with its files.
fn checkout_path(root: &Path, search_root: &Path) -> Option<Vec<u8>> {
    let normalize = crate::inspect::job::normalize_path;
    let root = normalize(root);
    let canonical = crate::bounded_io::canonicalize(search_root)
        .map(|path| normalize(&path))
        .ok();
    let relative = canonical
        .as_deref()
        .and_then(|path| path.strip_prefix(&root).ok())
        .map(Path::to_path_buf)
        .or_else(|| {
            normalize(search_root)
                .strip_prefix(&root)
                .ok()
                .map(Path::to_path_buf)
        })?;
    let mut path = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        if !path.is_empty() {
            path.push(b'/');
        }
        #[cfg(unix)]
        path.extend_from_slice(std::os::unix::ffi::OsStrExt::as_bytes(part));
        #[cfg(not(unix))]
        path.extend_from_slice(part.to_str()?.as_bytes());
    }
    Some(path)
}

/// The absolute path of checkout path `path` under `root`, spelled with the
/// platform's separators as a filesystem walk spells it.
fn absolute(root: &Path, path: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(root.join(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(path)))
    }
    #[cfg(not(unix))]
    {
        let mut absolute = root.to_path_buf();
        for part in std::str::from_utf8(path).ok()?.split('/') {
            absolute.push(part);
        }
        Some(absolute)
    }
}

/// A recorded modification time, in nanoseconds from the Unix epoch.
fn modified_at(mtime_ns: i64) -> SystemTime {
    let offset = Duration::from_nanos(mtime_ns.unsigned_abs());
    if mtime_ns >= 0 {
        UNIX_EPOCH.checked_add(offset)
    } else {
        UNIX_EPOCH.checked_sub(offset)
    }
    .unwrap_or(UNIX_EPOCH)
}

/// What tests use to run grep and glob the way they ran before the data
/// layer, in a process that runs it, and to see whether a call read it.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        static TODAY_ONLY: Cell<bool> = const { Cell::new(false) };
        static ANSWERS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn today_only() -> bool {
        TODAY_ONLY.with(Cell::get)
    }

    pub(super) fn note_answer() {
        ANSWERS.with(|answers| answers.set(answers.get() + 1));
    }
}
