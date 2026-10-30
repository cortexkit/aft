//! Files that must not be opened as project source by indexers and walkers.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

fn storage_roots() -> &'static RwLock<BTreeSet<PathBuf>> {
    static ROOTS: OnceLock<RwLock<BTreeSet<PathBuf>>> = OnceLock::new();
    ROOTS.get_or_init(RwLock::default)
}

/// Retain resolved storage roots even after reconfiguration: old readers and
/// databases can remain alive while a new route starts using another root.
pub(crate) fn register_storage_root(path: &Path) {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        return;
    };
    if storage_roots()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(&absolute)
    {
        // The absolute spelling is retained together with its canonical one,
        // so a repeat registration adds nothing and can skip the realpath
        // probe that produced them.
        return;
    }
    #[cfg(test)]
    REGISTER_CANONICALIZATIONS.with(|count| count.set(count.get() + 1));
    let canonical =
        crate::production_storage::canonicalized_with(&absolute, &|p| p.canonicalize().ok());
    let mut roots = storage_roots().write().unwrap_or_else(|p| p.into_inner());
    roots.insert(absolute);
    roots.insert(canonical);
}

/// A walk-scoped boundary that skips nested linked worktrees of this
/// repository: directories under the project root whose `.git` is a file
/// naming this repository's common git dir `worktrees/` entry (the layout
/// `git worktree add` creates). Indexing those checkouts again duplicates
/// every file, so every content-reading walker prunes them; an explicitly
/// named target inside one still walks, because the boundary only prunes
/// entries below the walk's own root, never the root itself.
///
/// Not a worktree root: a nested independent repository (its own `.git`
/// directory, or a `.git` file naming another common dir), a submodule
/// (its gitdir lives under `.git/modules/`, not `worktrees/`), and the
/// boundary's own root.
///
/// The check is cheap by construction: a directory is only examined past one
/// `symlink_metadata` when it holds a `.git` entry that is a file, the
/// repository's common dir is resolved once per boundary, and each verdict is
/// cached per directory for the boundary's life (one walk, or one watcher
/// filter call).
pub(crate) struct NestedWorktreeBoundary {
    root: PathBuf,
    common_dir: OnceLock<Option<PathBuf>>,
    verdicts: Mutex<HashMap<PathBuf, bool>>,
}

impl NestedWorktreeBoundary {
    /// One boundary per walk, rooted at the walk's root. The root itself is
    /// never reported, so a project opened inside a worktree still walks.
    pub(crate) fn for_root(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            common_dir: OnceLock::new(),
            verdicts: Mutex::new(HashMap::new()),
        }
    }

    fn common_dir(&self) -> Option<&Path> {
        self.common_dir
            .get_or_init(|| project_git_common_dir(&self.root))
            .as_deref()
    }

    /// True when `dir` itself is a nested linked worktree of this repository.
    /// Only directories below the walk root are ever reported; callers check
    /// `entry.depth() > 0` (or an equivalent) so the named root of an
    /// explicitly targeted walk is always admitted.
    pub(crate) fn is_nested_worktree_root(&self, dir: &Path) -> bool {
        if dir == self.root {
            return false;
        }
        if let Some(verdict) = self
            .verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dir)
        {
            return *verdict;
        }
        // Gate the `.git` file read on the entry existing as a file: ordinary
        // directories cost one metadata probe and never resolve the common dir.
        let verdict = std::fs::symlink_metadata(dir.join(".git"))
            .is_ok_and(|metadata| metadata.is_file())
            && self
                .common_dir()
                .is_some_and(|common| worktree_gitdir_points_at(dir, common));
        self.verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dir.to_path_buf(), verdict);
        verdict
    }

    /// True when `path` is at or under a nested linked worktree of this
    /// repository. The boundary root itself never counts, even when the
    /// project is opened inside a worktree.
    pub(crate) fn is_under_nested_worktree(&self, path: &Path) -> bool {
        let mut current = Some(path);
        while let Some(dir) = current {
            if dir == self.root || !dir.starts_with(&self.root) {
                return false;
            }
            if self.is_nested_worktree_root(dir) {
                return true;
            }
            current = dir.parent();
        }
        false
    }
}

/// This repository's common git dir for walks rooted at `root`: the `.git`
/// directory of a main checkout, the `commondir` target of a linked-worktree
/// checkout, or a submodule admin dir for a submodule checkout. `None` when
/// no ancestor holds `.git`, so there is no repository whose worktrees could
/// be nested below the root.
fn project_git_common_dir(root: &Path) -> Option<PathBuf> {
    let mut current = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(root)
    };
    loop {
        let dot_git = current.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            Ok(metadata) if metadata.is_dir() => return canonicalized(&dot_git),
            Ok(metadata) if metadata.is_file() => {
                let target = read_gitdir_target(&dot_git)?;
                let target = if target.is_absolute() {
                    target
                } else {
                    current.join(target)
                };
                // A linked-worktree checkout names its admin dir, which carries
                // a `commondir` file pointing at the shared git dir. Anything
                // else (a submodule admin dir, for example) is its own common
                // dir.
                if let Some(common) = read_commondir_target(&target) {
                    let common = if common.is_absolute() {
                        common
                    } else {
                        target.join(common)
                    };
                    return canonicalized(&common);
                }
                return canonicalized(&target);
            }
            Ok(_) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = current.parent().map(Path::to_path_buf) else {
                    return None;
                };
                current = parent;
            }
            Err(_) => return None,
        }
    }
}

/// True when `dir/.git` is a file whose `gitdir:` resolves to
/// `<common_dir>/worktrees/<name>`. Relative gitdir lines resolve against
/// `dir`, matching git; both sides are canonical, so symlinked checkouts and
/// platform spellings compare equal.
fn worktree_gitdir_points_at(dir: &Path, common_dir: &Path) -> bool {
    let target = match read_gitdir_target(&dir.join(".git")) {
        Some(target) => target,
        None => return false,
    };
    let target = if target.is_absolute() {
        target
    } else {
        dir.join(target)
    };
    worktree_common_candidate(&target).is_some_and(|candidate| candidate == *common_dir)
}

/// The canonical common dir when `gitdir_target` names a linked-worktree
/// admin dir (`<common>/worktrees/<name>`); `None` for submodule-style and
/// other gitdirs. Pure path logic, so Windows spellings (forward slashes,
/// drive paths) are covered without touching the filesystem.
fn worktree_common_candidate(gitdir_target: &Path) -> Option<PathBuf> {
    let worktrees = gitdir_target.parent()?;
    if worktrees.file_name()? != std::ffi::OsStr::new("worktrees") {
        return None;
    }
    canonicalized(worktrees.parent()?)
}

fn canonicalized(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Parse a `.git` file's `gitdir:` line into its (possibly relative) target.
/// The read is capped: a `.git` file is one short line, never bulk data.
fn read_gitdir_target(dot_git: &Path) -> Option<PathBuf> {
    parse_gitdir_bytes(&capped_read(dot_git, 4096)?)
}

/// The `commondir` file a linked worktree's gitdir carries, naming the shared
/// git dir (absolute, or relative to the gitdir).
fn read_commondir_target(gitdir: &Path) -> Option<PathBuf> {
    let bytes = capped_read(&gitdir.join("commondir"), 4096)?;
    let line = bytes.split(|byte| *byte == b'\n').next()?;
    let trimmed = trim_ascii_whitespace(line);
    if trimmed.is_empty() {
        return None;
    }
    bytes_to_path(trimmed)
}

fn parse_gitdir_bytes(bytes: &[u8]) -> Option<PathBuf> {
    let line = bytes.split(|byte| *byte == b'\n').next()?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = trim_ascii_whitespace(line);
    let rest = trim_ascii_whitespace(line.strip_prefix(b"gitdir:")?);
    if rest.is_empty() {
        return None;
    }
    bytes_to_path(rest)
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = bytes.split_last() {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(bytes).ok().map(PathBuf::from)
}

fn capped_read(path: &Path, cap: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}
// Test hook counting realpath probes for storage-root registration. The
// health rollup resolves process-wide storage paths once per tick, so one
// tick over many contexts sharing a root must barely move this count.
#[cfg(test)]
thread_local! {
    static REGISTER_CANONICALIZATIONS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn register_canonicalizations_for_test() -> u64 {
    REGISTER_CANONICALIZATIONS.with(|count| count.get())
}

#[cfg(test)]
pub(crate) fn reset_register_canonicalizations_for_test() {
    REGISTER_CANONICALIZATIONS.with(|count| count.set(0));
}

/// SQLite uses `-shm` files as shared-memory indexes and protects them with
/// process-wide file locks on macOS and Linux. Closing any descriptor to one of
/// these files, even a read-only indexing probe, releases SQLite's locks. A
/// second connection can then shrink the shared file and crash the first with
/// SIGBUS when it reads the old mapping. Reject databases and their sidecars
/// before binary detection, which would already open and close them. Internal
/// storage is also excluded wholesale so its generated source and cache files
/// never feed back into project indexing.
/// Walkers use the guarded readers' admission check to skip known live-file
/// aliases before scheduling source reads or reusing cached entries.
pub(crate) fn is_excluded(path: &Path) -> bool {
    admit(path, None).is_err()
}

pub(crate) fn is_excluded_with_file_type(
    path: &Path,
    file_type: Option<&std::fs::FileType>,
) -> bool {
    admit(path, file_type).is_err()
}

/// Watcher events can name vanished directories. A SQLite suffix only excludes
/// a known non-directory; an unknown type must still trigger reconciliation.
/// Unlike source walks, this check does not scan live SQLite identities.
pub(crate) fn is_excluded_event(path: &Path) -> bool {
    is_storage_path(path)
        || crate::os_metadata::is_os_metadata_path(path)
        || crate::db::file_identity::is_registered_file_path(path)
        || has_sqlite_suffix(path) && path.metadata().is_ok_and(|metadata| !metadata.is_dir())
}

fn is_storage_path(path: &Path) -> bool {
    storage_roots()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|root| path.starts_with(root))
}

/// Resolve aliases without opening file contents before checking retained stores.
pub(crate) fn storage_root_for_path(path: &Path) -> Option<PathBuf> {
    let path = crate::access_policy::policy_path(path);
    storage_roots()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|root| crate::access_policy::floor_contains(&path, root))
        .cloned()
}

fn is_excluded_file(path: &Path) -> bool {
    crate::os_metadata::is_os_metadata_path(path)
        || has_sqlite_suffix(path) && !path.is_dir()
        || crate::db::file_identity::is_registered_file_path(path)
}

fn has_sqlite_suffix(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.as_encoded_bytes();
        [
            b"-wal".as_slice(),
            b"-shm",
            b"-journal",
            b".db",
            b".sqlite",
            b".sqlite3",
        ]
        .iter()
        .any(|suffix| {
            name.len() >= suffix.len()
                && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        })
    })
}

fn is_excluded_file_with_metadata(
    path: &Path,
    file_type: Option<&std::fs::FileType>,
    metadata: Option<&std::fs::Metadata>,
) -> bool {
    crate::os_metadata::is_os_metadata_path(path)
        || has_sqlite_suffix(path)
            && !file_type.is_some_and(std::fs::FileType::is_dir)
            && !metadata.is_some_and(std::fs::Metadata::is_dir)
        || crate::db::file_identity::is_registered_file_path(path)
}

/// Authorized task content is not project source, but must still be checked
/// before a pinned-directory permission probe opens and closes its descriptor.
pub(crate) fn is_sqlite_or_metadata(path: &Path) -> bool {
    is_excluded_file(path) || crate::db::file_identity::is_live_sqlite_file(path)
}

/// Only explicit, ownership-checked registered task artifacts may bypass the
/// storage root exclusion. SQLite names and aliases remain forbidden.
pub(crate) fn read_bounded(
    path: &Path,
    deadline: Option<std::time::Instant>,
    authorized_task_artifact: bool,
) -> std::io::Result<Vec<u8>> {
    if authorized_task_artifact {
        if is_sqlite_or_metadata(path) || std::fs::symlink_metadata(path)?.file_type().is_symlink()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "SQLite files and aliases are not readable task content",
            ));
        }
    } else {
        admit(path, None)?;
    }
    #[cfg(test)]
    record_open(path);
    crate::bounded_io::read(path, deadline)
}

/// Guard direct source reads as well as enumeration. Watcher deltas and queued
/// view work can hold paths admitted before storage was configured.
pub(crate) fn read(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let path = path.as_ref();
    admit(path, None)?;
    #[cfg(test)]
    record_open(path);
    use std::io::Read;
    crate::views::io::WORK
        .content_reads
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut bytes = Vec::new();
    crate::task_temp_reads::open(path)?.read_to_end(&mut bytes)?;
    crate::views::io::WORK
        .content_read_bytes
        .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
    Ok(bytes)
}

pub(crate) fn read_to_string(path: impl AsRef<Path>) -> std::io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub(crate) fn open(path: impl AsRef<Path>) -> std::io::Result<std::fs::File> {
    let path = path.as_ref();
    admit(path, None)?;
    #[cfg(test)]
    record_open(path);
    crate::views::io::WORK
        .content_reads
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::task_temp_reads::open(path)
}

fn admit(path: &Path, file_type: Option<&std::fs::FileType>) -> std::io::Result<()> {
    // Walker types are enough for name/storage exclusions and for entries that
    // cannot alias SQLite. Regular files still need inode checks: a harmless
    // source name may be a hard link to a live database or a newly created sidecar.
    if crate::watcher_backend::flush::is_marker_path(path)
        || is_storage_path(path)
        || match file_type {
            Some(_) => is_excluded_file_with_metadata(path, file_type, None),
            None => {
                crate::os_metadata::is_os_metadata_path(path)
                    || crate::db::file_identity::is_registered_file_path(path)
            }
        }
    {
        return Err(excluded_error());
    }
    if file_type.is_some_and(|kind| !kind.is_file() && !kind.is_symlink()) {
        return Ok(());
    }
    let metadata = crate::db::file_identity::candidate_metadata(path).ok();
    if file_type.is_none()
        && has_sqlite_suffix(path)
        && !metadata.as_ref().is_some_and(std::fs::Metadata::is_dir)
    {
        return Err(excluded_error());
    }
    if metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) {
        return Ok(());
    }
    let excluded_alias = metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
        && path.canonicalize().is_ok_and(|target| is_excluded(&target));
    if excluded_alias
        || crate::db::file_identity::is_live_sqlite_file_with_metadata(path, metadata.as_ref())
    {
        Err(excluded_error())
    } else {
        Ok(())
    }
}

fn excluded_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "internal storage and SQLite files are not project source",
    )
}

/// Independent pre-optimization oracle for walker admission parity tests,
/// which run on Unix only.
#[cfg(all(test, unix))]
pub(crate) fn legacy_walker_excludes(path: &Path, file_type: Option<&std::fs::FileType>) -> bool {
    let known_directory = file_type.is_some_and(std::fs::FileType::is_dir);
    let metadata = (!known_directory)
        .then(|| crate::db::file_identity::candidate_metadata(path).ok())
        .flatten();
    if is_excluded_file_with_metadata(path, file_type, metadata.as_ref()) || is_storage_path(path) {
        return true;
    }
    if known_directory || metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) {
        return false;
    }
    let excluded_alias = metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
        && path
            .canonicalize()
            .is_ok_and(|target| legacy_walker_excludes(&target, None));
    excluded_alias
        || crate::db::file_identity::is_live_sqlite_file_with_metadata(path, metadata.as_ref())
}

#[cfg(test)]
mod open_probe {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    type Opens = Arc<Mutex<Vec<PathBuf>>>;
    fn probes() -> &'static Mutex<BTreeMap<PathBuf, Opens>> {
        static PROBES: OnceLock<Mutex<BTreeMap<PathBuf, Opens>>> = OnceLock::new();
        PROBES.get_or_init(Mutex::default)
    }

    pub(crate) struct Probe {
        root: PathBuf,
        opens: Opens,
    }

    impl Probe {
        pub(crate) fn new(root: &Path) -> Self {
            let opens = Arc::new(Mutex::new(Vec::new()));
            probes()
                .lock()
                .unwrap()
                .insert(root.to_path_buf(), opens.clone());
            Self {
                root: root.to_path_buf(),
                opens,
            }
        }

        pub(crate) fn paths(&self) -> Vec<PathBuf> {
            self.opens.lock().unwrap().clone()
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            probes().lock().unwrap().remove(&self.root);
        }
    }

    pub(crate) fn record_open(path: &Path) {
        for (root, opens) in probes().lock().unwrap().iter() {
            if path.starts_with(root) {
                opens.lock().unwrap().push(path.to_path_buf());
            }
        }
    }
}

#[cfg(test)]
pub(crate) use open_probe::{record_open, Probe};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ordinary_project_reads_do_not_stat_registered_databases() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let databases = (0..20)
            .map(|index| {
                crate::db::file_identity::IdentityConnection::open(
                    root.join(format!("database_{index}.cache")),
                    "source_read_stat_count",
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let source = root.join("source.rs");
        std::fs::write(&source, "fn source() {}\n").unwrap();
        let counts = crate::db::file_identity::stat_probe::count(|| {
            for _ in 0..1_000 {
                assert_eq!(read_to_string(&source).unwrap(), "fn source() {}\n");
            }
        });
        let candidate_stats = counts.get(&source).copied().unwrap_or(0);
        let registry_stats = counts
            .iter()
            .filter(|(path, _)| **path != source)
            .map(|(_, count)| count)
            .sum::<usize>();
        println!("1,000 source reads / 20 databases: {candidate_stats} candidate identity stats, {registry_stats} registry identity stats");
        assert_eq!(
            registry_stats, 0,
            "source reads stat registered database files"
        );
        assert_eq!(candidate_stats, 1_000);
        drop(databases);
    }

    #[test]
    fn sqlite_named_directories_remain_project_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut expected = Vec::new();
        for name in [
            "models.db",
            "models.SQLITE",
            "models.sqlite3",
            "event-wal",
            "event-shm",
            "event-journal",
        ] {
            let directory = root.join(name);
            std::fs::create_dir(&directory).unwrap();
            let source = directory.join("source.rs");
            std::fs::write(&source, "fn user_source() {}\n").unwrap();
            assert!(!is_excluded(&directory), "excluded directory {directory:?}");
            assert!(!is_excluded(&source));
            expected.push(source);
        }
        let storage = root.join("internal.db");
        std::fs::create_dir(&storage).unwrap();
        register_storage_root(&storage);
        std::fs::write(storage.join("internal.rs"), "fn internal() {}\n").unwrap();
        assert!(is_excluded(&storage));
        let mut walked = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        walked.sort();
        expected.sort();
        assert_eq!(walked, expected);
        for name in [
            "fixture.db",
            "fixture.sqlite",
            "fixture.sqlite3",
            "fixture-wal",
            "fixture-shm",
            "fixture-journal",
        ] {
            let file = root.join(name);
            std::fs::write(&file, "not source").unwrap();
            assert!(is_excluded(&file), "admitted file {file:?}");
        }
    }

    #[test]
    fn direct_project_reads_reject_storage_and_sqlite_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        register_storage_root(&storage);
        let probe = Probe::new(&root);
        for path in [
            storage.join("internal.rs"),
            root.join("live.db-shm"),
            root.join("LIVE.SQLITE-WAL"),
        ] {
            std::fs::write(&path, "valid UTF-8 must still not be opened").unwrap();
            assert!(read(&path).is_err(), "admitted {path:?}");
            assert!(open(&path).is_err(), "opened {path:?}");
            assert!(read_to_string(&path).is_err(), "decoded {path:?}");
            assert!(crate::inspect::scanners::source_text::read_source_text(&path).is_ok());
        }
        assert!(
            probe.paths().is_empty(),
            "opened excluded paths: {:?}",
            probe.paths()
        );
        let ordinary = root.join("storage-sibling.rs");
        std::fs::write(&ordinary, "fn user_source() {}\n").unwrap();
        assert!(!read(&ordinary).unwrap().is_empty());
        assert_eq!(probe.paths(), vec![ordinary]);
    }

    #[test]
    fn project_walks_and_watcher_skip_nested_storage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("state");
        std::fs::create_dir_all(storage.join("generated")).unwrap();
        register_storage_root(&storage);
        let internal = storage.join("generated/internal.rs");
        std::fs::write(&internal, "fn internal() {}\n").unwrap();
        let source = root.join("source.rs");
        std::fs::write(&source, "fn user_source() {}\n").unwrap();
        assert_eq!(
            crate::callgraph::walk_project_files(&root).collect::<Vec<_>>(),
            vec![source.clone()]
        );
        assert!(crate::watcher_filter::watcher_path_is_infra_skip_under(
            &root, &internal
        ));
        let probe = Probe::new(&root);
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1024,
        };
        let walk = crate::views::live_delta::strict_walk(&root, &policy, 1);
        assert!(walk.gaps.is_empty(), "{:?}", walk.gaps);
        assert_eq!(probe.paths(), vec![source]);
    }

    #[cfg(unix)]
    #[test]
    fn project_walks_do_not_probe_identity_for_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nested = root.join("one/two/three");
        std::fs::create_dir_all(&nested).unwrap();
        let directories = [root.join("one"), root.join("one/two"), nested.clone()];
        let source = nested.join("source.rs");
        std::fs::write(&source, "fn source() {}\n").unwrap();
        let database = crate::db::file_identity::IdentityConnection::open(
            root.join("open_database.cache"),
            "project_walk_directory_identity_probe",
        )
        .unwrap();

        let counts = crate::db::file_identity::stat_probe::count(|| {
            crate::search_index::project_walk_builder(&root)
                .build()
                .map(Result::unwrap)
                .for_each(drop);
        });

        for directory in directories {
            assert_eq!(
                counts.get(&directory),
                None,
                "walk probed directory identity for {directory:?}: {counts:?}"
            );
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = std::path::PathBuf::from(format!(
                "{}{}",
                root.join("open_database.cache").display(),
                suffix
            ));
            assert_eq!(
                counts.get(&sidecar),
                None,
                "directory visit refreshed database sidecar {sidecar:?}: {counts:?}"
            );
        }
        assert_eq!(counts.get(&source), Some(&1));
        drop(database);
    }

    #[cfg(unix)]
    #[test]
    fn late_created_shm_hard_link_is_refused_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("unconventional.cache");
        let connection =
            crate::db::file_identity::IdentityConnection::open(&database, "late_sidecar_read")
                .unwrap();
        let sidecar = root.join("unconventional.cache-shm");
        assert!(!sidecar.exists());
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES ('alive');").unwrap();
        assert!(sidecar.exists());
        let alias = root.join("innocent.rs");
        std::fs::hard_link(&sidecar, &alias).unwrap();
        let probe = Probe::new(&root);
        assert_eq!(
            read(&alias).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let symlink = root.join("symlink.rs");
        std::os::unix::fs::symlink(&alias, &symlink).unwrap();
        assert_eq!(
            open(&symlink).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(probe.paths().is_empty());
        assert_eq!(
            connection
                .query_row("SELECT value FROM probe", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "alive"
        );
    }

    // Windows cannot compare file identities here and refuses known SQLite names, not this `.rs` hard-link alias.
    #[cfg(unix)]
    #[test]
    fn live_sqlite_alias_walk_completes_without_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("live.db");
        let connection =
            crate::db::file_identity::IdentityConnection::open(&database, "walk_alias").unwrap();
        connection
            .execute_batch("CREATE TABLE probe(value TEXT)")
            .unwrap();
        let alias = root.join("alias.rs");
        std::fs::hard_link(&database, &alias).unwrap();
        let source = root.join("source.rs");
        std::fs::write(&source, "fn user_source() {}\n").unwrap();
        let probe = Probe::new(&root);
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1024,
        };
        let walk = crate::views::live_delta::strict_walk(&root, &policy, 1);
        assert!(walk.gaps.is_empty(), "{:?}", walk.gaps);
        assert_eq!(
            walk.entries
                .keys()
                .map(|path| path.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"source.rs".as_slice()]
        );
        assert_eq!(
            crate::callgraph::walk_project_files(&root).collect::<Vec<_>>(),
            vec![source.clone()]
        );
        let grep_files = crate::grep_executor::fallback_project_walk_builder(
            &root,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
        .build()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
        assert_eq!(grep_files, vec![source.clone()]);
        assert!(matches!(
            crate::inspect::scanners::source_text::read_source_text(&alias).unwrap(),
            crate::inspect::scanners::source_text::SourceText::Vanished
        ));
        assert_eq!(probe.paths(), vec![source]);
    }

    #[cfg(unix)]
    #[test]
    fn nonstandard_live_sqlite_names_are_excluded_by_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("unconventional.cache");
        let connection = crate::db::lifecycle::TrackedConnection::open(
            &database,
            crate::db::lifecycle::SqliteStore::AftDb,
        )
        .unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES ('alive');").unwrap();
        let probe = Probe::new(&root);
        assert!(is_excluded(&database));
        for (index, original) in [
            database.clone(),
            root.join("unconventional.cache-wal"),
            root.join("unconventional.cache-shm"),
        ]
        .iter()
        .enumerate()
        {
            let alias = root.join(format!("alias_{index}.rs"));
            std::fs::hard_link(original, &alias).unwrap();
            assert!(
                read(&alias).is_err(),
                "opened a live SQLite inode through {alias:?}"
            );
        }
        assert!(
            probe.paths().is_empty(),
            "opened live SQLite files: {:?}",
            probe.paths()
        );
        assert_eq!(
            connection
                .query_row("SELECT value FROM probe", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "alive"
        );
    }

    #[test]
    fn gitdir_lines_parse_absolute_relative_and_malformed() {
        // Absolute, relative, and CRLF-terminated lines resolve; anything else
        // is not a worktree marker and must not prune.
        assert_eq!(
            parse_gitdir_bytes(b"gitdir: /repo/.git/worktrees/wt\n"),
            Some(PathBuf::from("/repo/.git/worktrees/wt"))
        );
        assert_eq!(
            parse_gitdir_bytes(b"  gitdir:  ../.git/worktrees/wt  \r\n"),
            Some(PathBuf::from("../.git/worktrees/wt"))
        );
        for malformed in [
            b"gitdir:".as_slice(),
            b"gitdir:   \n",
            b"not a gitdir\n",
            b"",
            b"gitdir : /repo/.git/worktrees/wt\n",
        ] {
            assert_eq!(parse_gitdir_bytes(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn windows_gitdir_lines_name_a_worktrees_admin_dir() {
        // Worktree gitdir lines on Windows use forward slashes or drive paths.
        // Pure path logic, so this runs on every platform without git.
        for line in [
            "gitdir: C:/repo/.git/worktrees/wt\n",
            "gitdir: C:/repo/.git/worktrees/wt",
        ] {
            let target =
                parse_gitdir_bytes(line.as_bytes()).expect("Windows gitdir line must parse");
            assert_eq!(
                target.parent().and_then(|parent| parent.file_name()),
                Some(std::ffi::OsStr::new("worktrees"))
            );
            assert_eq!(
                target.parent().and_then(|parent| parent.parent()),
                Some(std::path::Path::new("C:/repo/.git"))
            );
        }
        // A submodule-style gitdir never names `worktrees/`.
        let target = parse_gitdir_bytes(b"gitdir: ../.git/modules/sub\n").unwrap();
        assert_ne!(
            target.parent().and_then(|parent| parent.file_name()),
            Some(std::ffi::OsStr::new("worktrees"))
        );
    }

    #[test]
    fn commondir_points_at_the_shared_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let common = root.join(".git");
        let admin = common.join("worktrees").join("wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("commondir"), "../../\n").unwrap();
        assert_eq!(read_commondir_target(&admin), Some(PathBuf::from("../../")));
        assert_eq!(read_commondir_target(&common), None);
    }

    #[test]
    fn hand_rolled_worktree_layout_resolves_without_git() {
        // The same layout `git worktree add` creates, without needing git:
        // main/.git is the common dir, main/linked/.git names its admin entry.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main");
        let linked = main.join("linked");
        let admin = main.join(".git").join("worktrees").join("linked");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let boundary = NestedWorktreeBoundary::for_root(&main);
        assert!(boundary.is_nested_worktree_root(&linked));
        assert!(!boundary.is_nested_worktree_root(&main));
        assert!(!boundary.is_nested_worktree_root(&root));
        assert!(boundary.is_under_nested_worktree(&linked.join("file.rs")));
        assert!(!boundary.is_under_nested_worktree(&main.join("file.rs")));
    }
}
