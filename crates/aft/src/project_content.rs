//! Files that must not be opened as project source by indexers and walkers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

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
    let canonical =
        crate::production_storage::canonicalized_with(&absolute, &|p| p.canonicalize().ok());
    let mut roots = storage_roots().write().unwrap_or_else(|p| p.into_inner());
    roots.insert(absolute);
    roots.insert(canonical);
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
}
