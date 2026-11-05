//! Building a checkout's state from its working tree.
//!
//! A scan walks the checkout, keeps every file its ignore rules admit, and
//! records each one's content key. A file whose stat evidence still matches
//! the previous state keeps its recorded key without being read. Every other
//! file is read and hashed once and handed to a [`ContentSink`], which
//! indexes what its planes do not hold yet.
//!
//! What the scan never does: follow a symlink, cross onto another
//! filesystem, enter a `.git` directory or a linked worktree of the same
//! repository nested inside the checkout, or open a SQLite database or one
//! of its side files (opening any of those from a process that holds a
//! connection drops that process's locks on them).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ignore::{WalkBuilder, WalkState};

use crate::checkout::{CheckoutState, PathKind, PathRecord, StatEvidence, StateError};
use crate::ids::{ContentKey, Digest128};
use crate::stat::observe;

/// What a scan admits and how much of a file it indexes.
#[derive(Clone, Debug)]
pub struct ScanPolicy {
    /// Files larger than this have no text index; unless `read_large`
    /// admits their path, their bytes are not read at all.
    pub max_indexed_len: u64,
    /// Paths `read_large` admits are read up to this size even past
    /// `max_indexed_len`, and handed to the sink as
    /// [`ContentKind::Large`]; larger ones are not read.
    pub max_read_len: u64,
    /// Which paths (relative to the root) are read past `max_indexed_len`,
    /// such as source files a plane parses. `None` reads none.
    pub read_large: Option<fn(&[u8]) -> bool>,
    /// Ignore files read beside `.gitignore`, such as `.aftignore`.
    pub ignore_files: Vec<String>,
    /// File names that are never members, such as `.DS_Store`.
    pub skip_names: Vec<String>,
    /// Directory names whose contents are never members, whether or not an
    /// ignore file names them: dependency, build and environment folders.
    /// The checkout's root itself is never skipped.
    pub skip_dirs: Vec<String>,
}

impl Default for ScanPolicy {
    fn default() -> Self {
        Self {
            max_indexed_len: 1024 * 1024,
            max_read_len: 4 * 1024 * 1024,
            read_large: None,
            ignore_files: vec![".aftignore".to_string()],
            skip_names: [".DS_Store", "Thumbs.db", "desktop.ini", "Icon\r"]
                .map(String::from)
                .to_vec(),
            // The folders AFT's project walk has always left out.
            skip_dirs: [
                "node_modules",
                "target",
                "venv",
                ".venv",
                "__pycache__",
                ".tox",
                "dist",
                "build",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

/// Whether a read content is text or binary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContentKind {
    Text,
    Binary,
    /// Text larger than the text-index limit, read for a path the policy's
    /// `read_large` admits: planes that parse read it, the text index
    /// does not index it.
    Large,
}

/// How much of a file a scan's policy reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SizeClass {
    /// Read, and indexed when text.
    Indexed,
    /// Read past the text-index limit; never indexed.
    Large,
    /// Not read.
    Unread,
}

impl ScanPolicy {
    fn class(&self, relative: &[u8], size: u64) -> SizeClass {
        if size <= self.max_indexed_len {
            SizeClass::Indexed
        } else if size <= self.max_read_len && self.read_large.is_some_and(|admit| admit(relative))
        {
            SizeClass::Large
        } else {
            SizeClass::Unread
        }
    }

    /// The class `record` was scanned under, by what it holds: a policy
    /// whose limits changed since then may classify the same file anew.
    fn class_of(&self, record: &PathRecord) -> SizeClass {
        match record.kind {
            PathKind::Oversize => SizeClass::Unread,
            PathKind::Large => SizeClass::Large,
            // A binary read past the text-index limit was read as large.
            PathKind::Binary if record.stat.size > self.max_indexed_len => SizeClass::Large,
            PathKind::Text | PathKind::Binary | PathKind::Unreadable | PathKind::Symlink => {
                SizeClass::Indexed
            }
        }
    }
}

/// Receives every file a scan reads, from several threads at once.
///
/// The same content can arrive once per path that holds it. Each plane
/// decides what identifies its own work (the text plane needs a content
/// once; parsing needs it once per language, which the path decides) and
/// skips what it already holds.
pub trait ContentSink: Sync {
    fn read(&self, path: &[u8], key: ContentKey, kind: ContentKind, bytes: &[u8]);
}

/// What a scan did, for status and for the cost gates.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScanCounters {
    pub files: usize,
    /// Files whose content key came from the previous state unread.
    pub reused: usize,
    /// Files read, hashed and handed to the sink.
    pub read: usize,
    pub bytes_read: u64,
    pub oversize: usize,
    pub unreadable: usize,
    /// Files that changed while being read; their records are marked racy
    /// so the next scan reads them again.
    pub changed_while_read: usize,
}

/// A checkout's state and how it was built.
#[derive(Debug)]
pub struct Scan {
    pub state: CheckoutState,
    pub counters: ScanCounters,
}

/// Why a scan failed as a whole. Problems with single files are recorded
/// in the state instead.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("cannot walk {path}: {source}")]
    Walk { path: PathBuf, source: io::Error },
    #[error("a path under the checkout is not valid UTF-8: {0}")]
    NotUtf8(PathBuf),
    #[error(transparent)]
    State(#[from] StateError),
}

/// Scans the checkout at `root`.
///
/// # Errors
///
/// When the root cannot be walked, or a path cannot be stored.
pub fn scan(
    root: &Path,
    policy: &ScanPolicy,
    previous: Option<&CheckoutState>,
    sink: &dyn ContentSink,
) -> Result<Scan, ScanError> {
    let (records, mut counters) = walk_records(root, policy, previous, None, sink)?;
    counters.files = records.len();
    let state = CheckoutState::new(Vec::new(), Digest128::of(b""), records)?.with_root(root);
    Ok(Scan { state, counters })
}

/// What [`scan_paths`] found: the checkout's whole state with the given
/// paths brought up to date, and which paths' records changed.
#[derive(Debug)]
pub struct PathScan {
    pub state: CheckoutState,
    /// Paths whose record was added, removed, or changed in content, kind,
    /// mode or link target, in byte order.
    pub changed: Vec<Vec<u8>>,
    pub counters: ScanCounters,
}

/// Brings `previous` up to date for `paths` (relative, such as a watcher or
/// an edit reports them): each path, and everything under it when it is a
/// folder, is walked again under exactly the rules a full [`scan`] applies.
/// A path that is gone, ignored, or inside a skipped folder leaves the
/// state; a folder that appeared is walked whole. Every other record is kept
/// as it is. Files are read only when their stat no longer proves the
/// recorded content.
///
/// # Errors
///
/// As [`scan`].
pub fn scan_paths(
    root: &Path,
    policy: &ScanPolicy,
    previous: &CheckoutState,
    paths: &[Vec<u8>],
    sink: &dyn ContentSink,
) -> Result<PathScan, ScanError> {
    let restrict = Arc::new(Restrict::new(paths));
    let (walked, mut counters) = walk_records(
        root,
        policy,
        Some(previous),
        Some(Arc::clone(&restrict)),
        sink,
    )?;
    let mut records = previous
        .paths()
        .iter()
        .filter(|record| !restrict.covers(&record.path))
        .cloned()
        .collect::<Vec<_>>();
    let mut changed = BTreeSet::new();
    for record in previous.paths() {
        if restrict.covers(&record.path) {
            changed.insert(record.path.clone());
        }
    }
    for record in &walked {
        changed.insert(record.path.clone());
    }
    let walked_by_path = walked
        .iter()
        .map(|record| (record.path.as_slice(), record))
        .collect::<std::collections::HashMap<_, _>>();
    changed.retain(|path| {
        let before = previous.get(path);
        let after = walked_by_path.get(path.as_slice()).copied();
        match (before, after) {
            (Some(before), Some(after)) => !same_member(before, after),
            (None, None) => false,
            _ => true,
        }
    });
    drop(walked_by_path);
    records.extend(walked);
    counters.files = records.len();
    let state =
        CheckoutState::new(previous.git.clone(), previous.conversion, records)?.with_root(root);
    Ok(PathScan {
        state,
        changed: changed.into_iter().collect(),
        counters,
    })
}

/// Whether two records of one path describe the same member: stat evidence
/// alone may differ.
fn same_member(before: &PathRecord, after: &PathRecord) -> bool {
    before.kind == after.kind
        && before.content == after.content
        && before.mode == after.mode
        && before.link == after.link
}

/// The paths a [`scan_paths`] walk is limited to.
#[derive(Debug)]
struct Restrict {
    paths: BTreeSet<Vec<u8>>,
}

impl Restrict {
    fn new(paths: &[Vec<u8>]) -> Self {
        Self {
            paths: paths
                .iter()
                .map(|path| {
                    let mut path = path.clone();
                    while path.last() == Some(&b'/') {
                        path.pop();
                    }
                    path
                })
                .filter(|path| !path.is_empty())
                .collect(),
        }
    }

    /// Whether `path` is one of the paths or under one of them.
    fn covers(&self, path: &[u8]) -> bool {
        if self.paths.contains(path) {
            return true;
        }
        path.iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .any(|(index, _)| self.paths.contains(&path[..index]))
    }

    /// Whether the walk must enter or keep `path`: it is covered, or it is a
    /// folder on the way to a covered path.
    fn admits(&self, path: &[u8]) -> bool {
        if self.covers(path) {
            return true;
        }
        let mut prefix = path.to_vec();
        prefix.push(b'/');
        self.paths
            .range(prefix.clone()..)
            .next()
            .is_some_and(|next| next.starts_with(&prefix))
    }
}

/// Walks `root` under the scan's rules, limited to `restrict` when given,
/// and records every member it reaches.
fn walk_records(
    root: &Path,
    policy: &ScanPolicy,
    previous: Option<&CheckoutState>,
    restrict: Option<Arc<Restrict>>,
    sink: &dyn ContentSink,
) -> Result<(Vec<PathRecord>, ScanCounters), ScanError> {
    let common_dir = git_common_dir(root);
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Inside a repository, Git's rules stop at its root, as Git's do: an
        // ignore file in a folder above it (a checkout placed inside another,
        // or a home folder under version control) changes nothing. Outside
        // one, `.gitignore` files still apply, from the root and above, as
        // the project walk and the watcher's filter apply them.
        .require_git(inside_work_tree(root))
        .follow_links(false)
        .same_file_system(true);
    for name in &policy.ignore_files {
        builder.add_custom_ignore_filename(name);
    }
    let skip_names = policy.skip_names.clone();
    let skip_dirs = policy.skip_dirs.clone();
    let nested_root = root.to_path_buf();
    builder.filter_entry(move |entry| {
        let name = entry.file_name();
        if name == ".git" || is_sqlite_file(name) {
            return false;
        }
        if skip_names.iter().any(|skip| OsStr::new(skip) == name) {
            return false;
        }
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        if is_dir && entry.depth() > 0 && skip_dirs.iter().any(|skip| OsStr::new(skip) == name) {
            return false;
        }
        if is_dir
            && entry.depth() > 0
            && entry.path() != nested_root
            && is_nested_worktree(entry.path(), common_dir.as_deref())
        {
            return false;
        }
        match (&restrict, entry.path().strip_prefix(&nested_root)) {
            (Some(restrict), Ok(relative)) if entry.depth() > 0 => {
                restrict.admits(&os_bytes(relative.as_os_str()))
            }
            _ => true,
        }
    });

    let records = Mutex::new(Vec::new());
    let walk_error = Mutex::new(None);
    let counters = Counters::default();
    builder.build_parallel().run(|| {
        Box::new(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    // A directory that cannot be listed is a hole in the
                    // state, so the whole scan fails rather than silently
                    // dropping its files.
                    if let Some(io) = error.io_error()
                        && io.kind() != io::ErrorKind::NotFound
                    {
                        let path = error_path(&error).unwrap_or_else(|| root.to_path_buf());
                        let mut slot = walk_error.lock().expect("no panics while holding it");
                        slot.get_or_insert(ScanError::Walk {
                            path,
                            source: io::Error::new(io.kind(), io.to_string()),
                        });
                        return WalkState::Quit;
                    }
                    return WalkState::Continue;
                }
            };
            let file_type = entry.file_type();
            let is_link = file_type.is_some_and(|kind| kind.is_symlink());
            if !is_link && !file_type.is_some_and(|kind| kind.is_file()) {
                return WalkState::Continue;
            }
            let relative = match relative_bytes(root, entry.path()) {
                Ok(relative) => relative,
                Err(error) => {
                    walk_error
                        .lock()
                        .expect("no panics while holding it")
                        .get_or_insert(error);
                    return WalkState::Quit;
                }
            };
            let previous = previous.and_then(|state| state.get(&relative));
            let record = if is_link {
                record_symlink(entry.path(), relative)
            } else {
                record_file(entry.path(), relative, previous, policy, sink, &counters)
            };
            if let Some(record) = record {
                records
                    .lock()
                    .expect("no panics while holding it")
                    .push(record);
            }
            WalkState::Continue
        })
    });
    if let Some(error) = walk_error.into_inner().expect("no panics while holding it") {
        return Err(error);
    }
    let records = records.into_inner().expect("no panics while holding it");
    Ok((records, counters.finish()))
}

#[derive(Default)]
struct Counters {
    reused: AtomicUsize,
    read: AtomicUsize,
    bytes_read: AtomicU64,
    oversize: AtomicUsize,
    unreadable: AtomicUsize,
    changed_while_read: AtomicUsize,
}

impl Counters {
    fn finish(self) -> ScanCounters {
        ScanCounters {
            files: 0,
            reused: self.reused.into_inner(),
            read: self.read.into_inner(),
            bytes_read: self.bytes_read.into_inner(),
            oversize: self.oversize.into_inner(),
            unreadable: self.unreadable.into_inner(),
            changed_while_read: self.changed_while_read.into_inner(),
        }
    }
}

/// The record of one file, or `None` when it vanished during the scan.
fn record_file(
    path: &Path,
    relative: Vec<u8>,
    previous: Option<&PathRecord>,
    policy: &ScanPolicy,
    sink: &dyn ContentSink,
    counters: &Counters,
) -> Option<PathRecord> {
    let stat = match observe(path) {
        Ok(Some(stat)) => stat,
        // Replaced by something that is not a file, or gone.
        Ok(None) => return None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => return Some(unreadable(relative, StatEvidence::default(), counters)),
    };
    let mode = file_mode(path);
    // Unchanged bytes are reused, but whether a file is read and indexed is
    // this scan's policy: a limit raised or lowered since the earlier scan
    // reclassifies the file, and an unread file may now have to be read.
    let class = policy.class(&relative, stat.size);
    if let Some(previous) = previous
        && previous.stat.proves_unchanged(&stat)
        && policy.class_of(previous) == class
    {
        counters.reused.fetch_add(1, Ordering::Relaxed);
        return Some(PathRecord {
            path: relative,
            mode,
            stat: previous.stat,
            ..previous.clone()
        });
    }
    if class == SizeClass::Unread {
        counters.oversize.fetch_add(1, Ordering::Relaxed);
        let content = ContentKey::stand_in(&relative, &evidence_bytes(&stat));
        return Some(PathRecord {
            path: relative,
            mode,
            kind: PathKind::Oversize,
            content,
            link: Vec::new(),
            stat,
            language: String::new(),
        });
    }
    let limit = match class {
        SizeClass::Large => policy.max_read_len,
        SizeClass::Indexed | SizeClass::Unread => policy.max_indexed_len,
    };
    let Ok(bytes) = read_bounded(path, limit) else {
        return Some(unreadable(relative, stat, counters));
    };
    counters.read.fetch_add(1, Ordering::Relaxed);
    if class == SizeClass::Large {
        counters.oversize.fetch_add(1, Ordering::Relaxed);
    }
    counters.bytes_read.fetch_add(
        bytes.as_ref().map_or(limit, |bytes| bytes.len() as u64),
        Ordering::Relaxed,
    );
    let mut stat = stat;
    match observe(path) {
        Ok(Some(after)) if same_file_state(&stat, &after) => {}
        Ok(None) => return None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        // Changed while read: the bytes may match neither stat, so the
        // record must not prove anything to the next scan.
        _ => {
            counters.changed_while_read.fetch_add(1, Ordering::Relaxed);
            stat.observed_ns = stat.mtime_ns.max(stat.ctime_ns);
        }
    }
    let Some(bytes) = bytes else {
        // Grew past the limit while being read.
        counters.oversize.fetch_add(1, Ordering::Relaxed);
        let content = ContentKey::stand_in(&relative, &evidence_bytes(&stat));
        return Some(PathRecord {
            path: relative,
            mode,
            kind: PathKind::Oversize,
            content,
            link: Vec::new(),
            stat,
            language: String::new(),
        });
    };
    let content = ContentKey::of(&bytes);
    let kind = if content_inspector::inspect(&bytes).is_binary() {
        ContentKind::Binary
    } else if bytes.len() as u64 > policy.max_indexed_len {
        ContentKind::Large
    } else {
        ContentKind::Text
    };
    sink.read(&relative, content, kind, &bytes);
    Some(PathRecord {
        path: relative,
        mode,
        kind: match kind {
            ContentKind::Text => PathKind::Text,
            ContentKind::Binary => PathKind::Binary,
            ContentKind::Large => PathKind::Large,
        },
        content,
        link: Vec::new(),
        stat,
        language: String::new(),
    })
}

/// The record of a symlink: its target, never followed. `None` when it
/// vanished.
fn record_symlink(path: &Path, relative: Vec<u8>) -> Option<PathRecord> {
    let target = match std::fs::read_link(path) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => PathBuf::new(),
    };
    let link = os_bytes(target.as_os_str());
    Some(PathRecord {
        content: ContentKey::stand_in(&relative, &link),
        path: relative,
        mode: 0o120_000,
        kind: PathKind::Symlink,
        link,
        stat: StatEvidence::default(),
        language: String::new(),
    })
}

fn unreadable(relative: Vec<u8>, stat: StatEvidence, counters: &Counters) -> PathRecord {
    counters.unreadable.fetch_add(1, Ordering::Relaxed);
    // Racy, so the next scan tries again.
    let stat = StatEvidence {
        observed_ns: stat.mtime_ns.max(stat.ctime_ns),
        ..stat
    };
    PathRecord {
        content: ContentKey::stand_in(&relative, &evidence_bytes(&stat)),
        path: relative,
        mode: 0,
        kind: PathKind::Unreadable,
        link: Vec::new(),
        stat,
        language: String::new(),
    }
}

/// Reads at most `limit` bytes; `None` when the file holds more.
fn read_bounded(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// Whether two observations describe the same file state, ignoring when
/// they were taken.
fn same_file_state(before: &StatEvidence, after: &StatEvidence) -> bool {
    StatEvidence {
        observed_ns: 0,
        ..*before
    } == StatEvidence {
        observed_ns: 0,
        ..*after
    }
}

fn evidence_bytes(stat: &StatEvidence) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&stat.size.to_le_bytes());
    out.extend_from_slice(&stat.mtime_ns.to_le_bytes());
    out.extend_from_slice(&stat.ctime_ns.to_le_bytes());
    out.extend_from_slice(&stat.file_id.to_le_bytes());
    out.extend_from_slice(&stat.device.to_le_bytes());
    out.extend_from_slice(&stat.usn.unwrap_or(0).to_le_bytes());
    out
}

/// Git's mode for a regular file: executable or not.
fn file_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0);
        if executable { 0o100_755 } else { 0o100_644 }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o100_644
    }
}

fn relative_bytes(root: &Path, path: &Path) -> Result<Vec<u8>, ScanError> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut out = Vec::new();
    for (index, component) in relative.components().enumerate() {
        if index > 0 {
            out.push(b'/');
        }
        let os = component.as_os_str();
        #[cfg(unix)]
        out.extend_from_slice(std::os::unix::ffi::OsStrExt::as_bytes(os));
        #[cfg(not(unix))]
        out.extend_from_slice(
            os.to_str()
                .ok_or_else(|| ScanError::NotUtf8(path.to_path_buf()))?
                .as_bytes(),
        );
    }
    Ok(out)
}

/// An OS string's bytes: raw on Unix, UTF-8 with `/` separators elsewhere.
fn os_bytes(os: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStrExt::as_bytes(os).to_vec()
    }
    #[cfg(not(unix))]
    {
        os.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

fn error_path(error: &ignore::Error) -> Option<PathBuf> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path.clone()),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        _ => None,
    }
}

/// SQLite databases and their side files. Opening one from a process that
/// holds a connection to it releases that process's locks on it.
fn is_sqlite_file(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    [".db", ".sqlite", ".sqlite3", "-wal", "-shm", "-journal"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// The common Git directory of the repository `root` is in, when `root` is
/// a Git checkout root.
/// Whether `path` or a folder above it holds `.git` (a folder, or the file a
/// linked worktree has) or `.jj`.
fn inside_work_tree(path: &Path) -> bool {
    path.ancestors()
        .any(|dir| dir.join(".git").symlink_metadata().is_ok() || dir.join(".jj").is_dir())
}

fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        gitdir_of_file(&dot_git)?
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => resolve(&git_dir, text.trim()),
        Err(_) => git_dir,
    };
    std::fs::canonicalize(common).ok()
}

/// Whether `dir` is a linked worktree of the repository whose common Git
/// directory is `common`: it has a `.git` file pointing into that
/// directory. Its files belong to its own checkout.
fn is_nested_worktree(dir: &Path, common: Option<&Path>) -> bool {
    let Some(common) = common else {
        return false;
    };
    gitdir_of_file(&dir.join(".git"))
        .and_then(|git_dir| std::fs::canonicalize(git_dir).ok())
        .is_some_and(|git_dir| git_dir.starts_with(common))
}

/// The directory a `.git` file points at.
fn gitdir_of_file(dot_git: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(dot_git).ok()?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return None;
    }
    let text = std::fs::read_to_string(dot_git).ok()?;
    let target = text.strip_prefix("gitdir:")?.trim();
    Some(resolve(dot_git.parent()?, target))
}

fn resolve(base: &Path, target: &str) -> PathBuf {
    let target = Path::new(target);
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::stat::RACY_WINDOW_NS;

    /// What the sink saw for one path: its key, kind and bytes.
    type Seen = (ContentKey, ContentKind, Vec<u8>);

    #[derive(Default)]
    struct Collect {
        read: Mutex<BTreeMap<Vec<u8>, Seen>>,
        calls: AtomicUsize,
    }

    impl ContentSink for Collect {
        fn read(&self, path: &[u8], key: ContentKey, kind: ContentKind, bytes: &[u8]) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(ContentKey::of(bytes), key);
            self.read
                .lock()
                .unwrap()
                .insert(path.to_vec(), (key, kind, bytes.to_vec()));
        }
    }

    fn paths(state: &CheckoutState) -> Vec<String> {
        state
            .paths()
            .iter()
            .map(|record| String::from_utf8_lossy(&record.path).into_owned())
            .collect()
    }

    /// Ages every record past the racy window, as if the scan ran long ago.
    fn aged(state: &CheckoutState) -> CheckoutState {
        let records = state
            .paths()
            .iter()
            .map(|record| PathRecord {
                stat: StatEvidence {
                    observed_ns: record.stat.observed_ns + 2 * RACY_WINDOW_NS,
                    ..record.stat
                },
                ..record.clone()
            })
            .collect();
        CheckoutState::new(Vec::new(), state.conversion, records).unwrap()
    }

    /// One member: its path, kind, content and link target.
    type Member = (String, PathKind, ContentKey, Vec<u8>);

    /// One step of a path-scan test: what it changes on disk, the paths a
    /// watcher would report, and the paths whose record must change.
    type Step<'a> = (Box<dyn Fn() + 'a>, Vec<&'a str>, Vec<&'a str>);

    /// The members of `state` with what identifies each, for comparing a
    /// path scan with a full scan.
    fn members(state: &CheckoutState) -> Vec<Member> {
        state
            .paths()
            .iter()
            .map(|record| {
                (
                    String::from_utf8_lossy(&record.path).into_owned(),
                    record.kind,
                    record.content,
                    record.link.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn a_path_scan_equals_a_full_scan_under_the_same_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |path: &str, bytes: &[u8]| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write(".gitignore", b"ignored/\n*.log\n");
        write("src/a.rs", b"fn a() {}\n");
        write("docs/one.txt", b"one\n");
        write("docs/two.txt", b"two\n");
        let policy = ScanPolicy {
            max_indexed_len: 2048,
            ..ScanPolicy::default()
        };
        let sink = Collect::default();
        let mut state = aged(&scan(root, &policy, None, &sink).unwrap().state);

        // Each step changes the tree, then brings the state up to date for
        // the paths a watcher would report, and must match a full scan.
        let mut late_nul = vec![b'x'; 1500];
        late_nul[1400] = 0;
        let steps: Vec<Step<'_>> = vec![
            (
                Box::new(|| write("src/a.rs", b"fn changed() {}\n")),
                vec!["src/a.rs"],
                vec!["src/a.rs"],
            ),
            (
                // Build output and ignored files never become members.
                Box::new(|| {
                    write("target/debug/gen.rs", b"fn gen() {}\n");
                    write("ignored/x.rs", b"no\n");
                    write("run.log", b"no\n");
                }),
                vec!["target/debug/gen.rs", "ignored/x.rs", "run.log", "target"],
                vec![],
            ),
            (
                Box::new(|| write("src/big.rs", &vec![b'y'; 4096])),
                vec!["src/big.rs"],
                vec!["src/big.rs"],
            ),
            (
                // A NUL past the first KiB is judged as the full scan judges it.
                Box::new(move || write("src/late.rs", &late_nul)),
                vec!["src/late.rs"],
                vec!["src/late.rs"],
            ),
            (
                // A removed folder takes its files with it.
                Box::new(|| std::fs::remove_dir_all(root.join("docs")).unwrap()),
                vec!["docs"],
                vec!["docs/one.txt", "docs/two.txt"],
            ),
            (
                // A folder that appears is walked whole.
                Box::new(|| {
                    write("lib/nested/deep.rs", b"fn deep() {}\n");
                    write("lib/top.rs", b"fn top() {}\n");
                }),
                vec!["lib"],
                vec!["lib/nested/deep.rs", "lib/top.rs"],
            ),
        ];
        for (index, (mutate, reported, expected)) in steps.into_iter().enumerate() {
            mutate();
            let reported = reported
                .iter()
                .map(|path| path.as_bytes().to_vec())
                .collect::<Vec<_>>();
            let partial = scan_paths(root, &policy, &state, &reported, &sink).unwrap();
            let full = scan(root, &policy, None, &sink).unwrap();
            assert_eq!(
                members(&partial.state),
                members(&full.state),
                "step {index}"
            );
            let changed = partial
                .changed
                .iter()
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .collect::<Vec<_>>();
            assert_eq!(changed, expected, "step {index}");
            state = aged(&partial.state);
        }
        assert_eq!(state.get(b"src/big.rs").unwrap().kind, PathKind::Oversize);
    }

    #[test]
    fn a_checkout_inside_another_ignores_the_outer_rules() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join(".gitignore"), b"vendor/\n*.log\n").unwrap();
        let root = outer.path().join("nested");
        let write = |path: &str, bytes: &[u8]| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write("vendor/lib.rs", b"fn lib() {}\n");
        write("run.log", b"log\n");
        // Not a repository: the outer `.gitignore` applies, as it does to the
        // project walk.
        let plain = scan(&root, &ScanPolicy::default(), None, &Collect::default()).unwrap();
        assert!(plain.state.get(b"vendor/lib.rs").is_none());
        // A linked worktree's `.git` file makes it a repository, whose rules
        // stop at its root.
        write(".git", b"gitdir: /nowhere\n");
        let checkout = scan(&root, &ScanPolicy::default(), None, &Collect::default()).unwrap();
        assert!(checkout.state.get(b"vendor/lib.rs").is_some());
        assert!(checkout.state.get(b"run.log").is_some());
    }

    /// Windows spells one folder several ways: with the `\\?\` prefix that
    /// `canonicalize` returns, without it, or with another drive-letter
    /// case. Every spelling of the root records the same members, and a
    /// linked worktree whose `.git` file names its Git folder as Git for
    /// Windows writes it (`C:/...`, forward slashes) stays out.
    #[cfg(windows)]
    #[test]
    fn every_spelling_of_a_windows_root_records_the_same_members() {
        let dir = tempfile::tempdir().unwrap();
        let verbatim = std::fs::canonicalize(dir.path()).unwrap();
        let spelled = verbatim.to_str().unwrap();
        let plain = PathBuf::from(
            spelled
                .strip_prefix(r"\\?\")
                .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
                .unwrap_or_else(|| panic!("not a verbatim drive path: {spelled}")),
        );
        let plain_spelled = plain.to_str().unwrap();
        let other_case = PathBuf::from(format!(
            "{}{}",
            if plain_spelled.as_bytes()[0].is_ascii_uppercase() {
                plain_spelled[..1].to_ascii_lowercase()
            } else {
                plain_spelled[..1].to_ascii_uppercase()
            },
            &plain_spelled[1..]
        ));
        let write = |path: &str, bytes: &[u8]| {
            let path = plain.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        std::fs::create_dir_all(plain.join(".git/worktrees/w")).unwrap();
        write(".gitignore", b"ignored/\n");
        write("src/a.rs", b"fn a() {}\n");
        write("Docs/Read Me.md", b"a space and capitals\n");
        write("ignored/x.rs", b"no");
        let worktree_git_dir = plain
            .join(".git")
            .join("worktrees")
            .join("w")
            .to_str()
            .unwrap()
            .replace('\\', "/");
        write("w/.git", format!("gitdir: {worktree_git_dir}\n").as_bytes());
        write("w/inner.rs", b"no");

        let policy = ScanPolicy::default();
        let sink = Collect::default();
        let first = scan(&plain, &policy, None, &sink).unwrap();
        assert_eq!(
            paths(&first.state),
            [".gitignore", "Docs/Read Me.md", "src/a.rs"]
        );
        for root in [&verbatim, &other_case] {
            let again = scan(root, &policy, None, &sink).unwrap();
            assert_eq!(
                members(&again.state),
                members(&first.state),
                "{}",
                root.display()
            );
            // A path scan from this spelling keeps every member as it is.
            let reported = [b"src".to_vec(), b"Docs/Read Me.md".to_vec(), b"w".to_vec()];
            let partial = scan_paths(root, &policy, &aged(&first.state), &reported, &sink).unwrap();
            assert_eq!(
                members(&partial.state),
                members(&first.state),
                "{}",
                root.display()
            );
            assert!(partial.changed.is_empty(), "{:?}", partial.changed);
        }
    }

    #[test]
    fn admits_what_the_rules_admit_and_reuses_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A repository, so folders above the temporary root cannot change
        // what it admits.
        std::fs::create_dir(root.join(".git")).unwrap();
        let write = |path: &str, bytes: &[u8]| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write(".gitignore", b"ignored/\n*.log\n");
        write(".aftignore", b"secret.txt\n");
        write("src/a.rs", b"fn a() {}\n");
        write("src/b.rs", b"fn a() {}\n");
        write("src/.hidden.rs", b"fn hidden() {}\n");
        write("ignored/x.rs", b"no");
        write("run.log", b"no");
        write("secret.txt", b"no");
        write("state.db", b"no");
        write("state.db-wal", b"no");
        write(".DS_Store", b"no");
        write("blob.bin", &[0, 159, 146, 150, 0, 0]);
        write("big.txt", &vec![b'x'; 2048]);
        write(".git/config", b"no");
        // A linked worktree of this repository nested inside it.
        std::fs::create_dir_all(root.join(".git/worktrees/w")).unwrap();
        write("w/.git", b"gitdir: ../.git/worktrees/w\n");
        write("w/inner.rs", b"no");
        // Another repository nested inside it stays a member.
        write("vendor/lib/.git/HEAD", b"ref: refs/heads/main\n");
        write("vendor/lib/lib.rs", b"pub fn lib() {}\n");
        // Dependency and build folders are never members, ignored or not;
        // a file merely named like one is.
        write("web/node_modules/pkg/index.js", b"no");
        write("crates/x/target/debug/out.rs", b"no");
        write("app/dist/bundle.js", b"no");
        write("tool/build/gen.rs", b"no");
        write("py/.venv/lib.py", b"no");
        write("docs/build", b"a file named build\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink("src/a.rs", root.join("link.rs")).unwrap();

        let policy = ScanPolicy {
            max_indexed_len: 1024,
            ..ScanPolicy::default()
        };
        let get = |state: &CheckoutState, path: &str| state.get(path.as_bytes()).unwrap().clone();
        let sink = Collect::default();
        let first = scan(root, &policy, None, &sink).unwrap();
        #[cfg(unix)]
        {
            let link = get(&first.state, "link.rs");
            assert_eq!(
                (link.kind, link.link.as_slice()),
                (PathKind::Symlink, &b"src/a.rs"[..])
            );
        }
        let mut expected = vec![
            ".aftignore",
            ".gitignore",
            "big.txt",
            "blob.bin",
            "docs/build",
            "src/.hidden.rs",
            "src/a.rs",
            "src/b.rs",
            "vendor/lib/lib.rs",
        ];
        if cfg!(unix) {
            expected.insert(5, "link.rs");
        }
        assert_eq!(paths(&first.state), expected);
        assert_eq!(get(&first.state, "big.txt").kind, PathKind::Oversize);
        assert_eq!(get(&first.state, "blob.bin").kind, PathKind::Binary);
        assert_eq!(
            get(&first.state, "src/a.rs").content,
            get(&first.state, "src/b.rs").content
        );
        // Every read file is handed over with its path; the oversize file is
        // never read. `docs/build` is the one added member.
        assert_eq!(sink.calls.load(Ordering::Relaxed), 8);
        assert_eq!(first.counters.read, 8);
        assert_eq!(
            sink.read.lock().unwrap()[&b"blob.bin"[..]].1,
            ContentKind::Binary
        );
        assert_eq!(first.counters.oversize, 1);

        // Unchanged files are reused unread; an edit is read again. On
        // Windows the change journal's update sequence number (USN) is what
        // proves them unchanged.
        write("src/a.rs", b"fn changed() {}\n");
        let second = scan(root, &policy, Some(&aged(&first.state)), &sink).unwrap();
        assert_eq!(second.counters.read, 1, "{:?}", second.counters);
        assert_eq!(second.counters.reused, 8);
        assert_ne!(
            get(&second.state, "src/a.rs").content,
            get(&first.state, "src/a.rs").content
        );

        // The file just read is still racy, so it proves nothing and is
        // read again; reused records keep their old evidence.
        let third = scan(root, &policy, Some(&second.state), &sink).unwrap();
        assert_eq!((third.counters.reused, third.counters.read), (8, 1));
        let contents = |state: &CheckoutState| {
            state
                .paths()
                .iter()
                .map(|record| (record.path.clone(), record.kind, record.content))
                .collect::<Vec<_>>()
        };
        assert_eq!(contents(&third.state), contents(&second.state));
    }

    #[test]
    fn a_changed_size_limit_reclassifies_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("big.txt"), vec![b'x'; 2048]).unwrap();
        std::fs::write(root.join("small.txt"), b"small").unwrap();
        let limit = |max_indexed_len| ScanPolicy {
            max_indexed_len,
            ..ScanPolicy::default()
        };
        let kind = |state: &CheckoutState, path: &str| state.get(path.as_bytes()).unwrap().kind;
        let sink = Collect::default();
        let first = scan(root, &limit(1024), None, &sink).unwrap();
        assert_eq!(kind(&first.state, "big.txt"), PathKind::Oversize);

        // Raising the limit reads the file that was too large, though its
        // bytes did not change; the other file is reused unread.
        let raised = scan(root, &limit(4096), Some(&aged(&first.state)), &sink).unwrap();
        assert_eq!(kind(&raised.state, "big.txt"), PathKind::Text);
        assert_eq!((raised.counters.read, raised.counters.reused), (1, 1));
        assert_eq!(
            sink.read.lock().unwrap()[&b"big.txt"[..]].2.len(),
            2048,
            "the reclassified file was handed over whole"
        );

        // Lowering it again makes it oversize without reading it.
        let lowered = scan(root, &limit(1024), Some(&aged(&raised.state)), &sink).unwrap();
        assert_eq!(kind(&lowered.state, "big.txt"), PathKind::Oversize);
        assert_eq!((lowered.counters.read, lowered.counters.reused), (0, 1));
    }

    fn admits_rust(path: &[u8]) -> bool {
        path.ends_with(b".rs")
    }

    #[test]
    fn admitted_paths_are_read_past_the_index_limit_up_to_the_read_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("big.rs"), vec![b'x'; 2048]).unwrap();
        std::fs::write(root.join("big.txt"), vec![b'x'; 2048]).unwrap();
        std::fs::write(root.join("huge.rs"), vec![b'x'; 8192]).unwrap();
        let policy = |read_large: Option<fn(&[u8]) -> bool>| ScanPolicy {
            max_indexed_len: 1024,
            max_read_len: 4096,
            read_large,
            ..ScanPolicy::default()
        };
        let get = |state: &CheckoutState, path: &str| state.get(path.as_bytes()).unwrap().clone();
        let sink = Collect::default();
        let first = scan(root, &policy(Some(admits_rust)), None, &sink).unwrap();

        // An admitted path past the index limit is read whole, with the key
        // of its bytes, and handed over as large text.
        let big = get(&first.state, "big.rs");
        assert_eq!(big.kind, PathKind::Large);
        assert_eq!(big.content, ContentKey::of(&vec![b'x'; 2048]));
        let read = sink.read.lock().unwrap()[&b"big.rs"[..]].clone();
        assert_eq!((read.1, read.2.len()), (ContentKind::Large, 2048));
        // A path not admitted, or one past the read limit, is not read.
        assert_eq!(get(&first.state, "big.txt").kind, PathKind::Oversize);
        assert_eq!(get(&first.state, "huge.rs").kind, PathKind::Oversize);
        assert!(!sink.read.lock().unwrap().contains_key(&b"huge.rs"[..]));
        assert_eq!(first.counters.read, 1);

        // Unchanged, the large file is reused unread with its key.
        let again = scan(
            root,
            &policy(Some(admits_rust)),
            Some(&aged(&first.state)),
            &sink,
        )
        .unwrap();
        assert_eq!(again.counters.read, 0);
        let reused = get(&again.state, "big.rs");
        assert_eq!((reused.kind, reused.content), (big.kind, big.content));

        // A policy that stops admitting it makes it unread, and admitting it
        // again reads it, though its bytes never changed.
        let unread = scan(root, &policy(None), Some(&aged(&again.state)), &sink).unwrap();
        assert_eq!(get(&unread.state, "big.rs").kind, PathKind::Oversize);
        assert_eq!(unread.counters.read, 0);
        let reread = scan(
            root,
            &policy(Some(admits_rust)),
            Some(&aged(&unread.state)),
            &sink,
        )
        .unwrap();
        assert_eq!(get(&reread.state, "big.rs").kind, PathKind::Large);
        assert_eq!(reread.counters.read, 1);

        // Raising the index limit past it makes it ordinary indexed text.
        let raised = ScanPolicy {
            max_indexed_len: 4096,
            ..policy(Some(admits_rust))
        };
        let indexed = scan(root, &raised, Some(&aged(&reread.state)), &sink).unwrap();
        assert_eq!(get(&indexed.state, "big.rs").kind, PathKind::Text);
        assert_eq!(
            indexed.counters.read, 2,
            "big.rs and big.txt are read again"
        );
    }
}
