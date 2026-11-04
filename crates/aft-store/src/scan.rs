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

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ignore::{WalkBuilder, WalkState};

use crate::checkout::{CheckoutState, PathKind, PathRecord, StatEvidence, StateError};
use crate::ids::{ContentKey, Digest128};
use crate::stat::observe;

/// What a scan admits and how much of a file it indexes.
#[derive(Clone, Debug)]
pub struct ScanPolicy {
    /// Files larger than this are members whose bytes are not read.
    pub max_indexed_len: u64,
    /// Ignore files read beside `.gitignore`, such as `.aftignore`.
    pub ignore_files: Vec<String>,
    /// File names that are never members, such as `.DS_Store`.
    pub skip_names: Vec<String>,
}

impl Default for ScanPolicy {
    fn default() -> Self {
        Self {
            max_indexed_len: 1024 * 1024,
            ignore_files: vec![".aftignore".to_string()],
            skip_names: [".DS_Store", "Thumbs.db", "desktop.ini", "Icon\r"]
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
    let common_dir = git_common_dir(root);
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .same_file_system(true);
    for name in &policy.ignore_files {
        builder.add_custom_ignore_filename(name);
    }
    let skip_names = policy.skip_names.clone();
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
        !(is_dir
            && entry.depth() > 0
            && entry.path() != nested_root
            && is_nested_worktree(entry.path(), common_dir.as_deref()))
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
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
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
            if let Some(record) =
                record_file(entry.path(), relative, previous, policy, sink, &counters)
            {
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
    let mut counters = counters.finish();
    counters.files = records.len();
    let state = CheckoutState::new(Vec::new(), Digest128::of(b""), records)?;
    Ok(Scan { state, counters })
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
    if let Some(previous) = previous
        && previous.stat.proves_unchanged(&stat)
    {
        counters.reused.fetch_add(1, Ordering::Relaxed);
        return Some(PathRecord {
            path: relative,
            mode,
            stat: previous.stat,
            ..previous.clone()
        });
    }
    if stat.size > policy.max_indexed_len {
        counters.oversize.fetch_add(1, Ordering::Relaxed);
        let content = ContentKey::stand_in(&relative, &evidence_bytes(&stat));
        return Some(PathRecord {
            path: relative,
            mode,
            kind: PathKind::Oversize,
            content,
            stat,
            language: String::new(),
        });
    }
    let Ok(bytes) = read_bounded(path, policy.max_indexed_len) else {
        return Some(unreadable(relative, stat, counters));
    };
    counters.read.fetch_add(1, Ordering::Relaxed);
    counters.bytes_read.fetch_add(
        bytes
            .as_ref()
            .map_or(policy.max_indexed_len, |bytes| bytes.len() as u64),
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
            stat,
            language: String::new(),
        });
    };
    let content = ContentKey::of(&bytes);
    let kind = if content_inspector::inspect(&bytes).is_binary() {
        ContentKind::Binary
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
        },
        content,
        stat,
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

    #[test]
    fn admits_what_the_rules_admit_and_reuses_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
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

        let policy = ScanPolicy {
            max_indexed_len: 1024,
            ..ScanPolicy::default()
        };
        let sink = Collect::default();
        let first = scan(root, &policy, None, &sink).unwrap();
        assert_eq!(
            paths(&first.state),
            [
                ".aftignore",
                ".gitignore",
                "big.txt",
                "blob.bin",
                "src/.hidden.rs",
                "src/a.rs",
                "src/b.rs",
                "vendor/lib/lib.rs"
            ]
        );
        let get = |state: &CheckoutState, path: &str| state.get(path.as_bytes()).unwrap().clone();
        assert_eq!(get(&first.state, "big.txt").kind, PathKind::Oversize);
        assert_eq!(get(&first.state, "blob.bin").kind, PathKind::Binary);
        assert_eq!(
            get(&first.state, "src/a.rs").content,
            get(&first.state, "src/b.rs").content
        );
        // Every read file is handed over with its path; the oversize file is
        // never read.
        assert_eq!(sink.calls.load(Ordering::Relaxed), 7);
        assert_eq!(first.counters.read, 7);
        assert_eq!(
            sink.read.lock().unwrap()[&b"blob.bin"[..]].1,
            ContentKind::Binary
        );
        assert_eq!(first.counters.oversize, 1);

        // Unchanged files are reused unread; an edit is read again.
        write("src/a.rs", b"fn changed() {}\n");
        let second = scan(root, &policy, Some(&aged(&first.state)), &sink).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(second.counters.read, 1, "{:?}", second.counters);
            assert_eq!(second.counters.reused, 7);
        }
        assert_ne!(
            get(&second.state, "src/a.rs").content,
            get(&first.state, "src/a.rs").content
        );

        // The file just read is still racy, so it proves nothing and is
        // read again; reused records keep their old evidence.
        let third = scan(root, &policy, Some(&second.state), &sink).unwrap();
        #[cfg(unix)]
        assert_eq!((third.counters.reused, third.counters.read), (7, 1));
        let contents = |state: &CheckoutState| {
            state
                .paths()
                .iter()
                .map(|record| (record.path.clone(), record.kind, record.content))
                .collect::<Vec<_>>()
        };
        assert_eq!(contents(&third.state), contents(&second.state));
    }
}
