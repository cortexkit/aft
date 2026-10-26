//! The storage root's monotonic reader floor.
//!
//! `<storage root>/reader-floor.json` records, for every versioned store AFT
//! persists, the lowest format version a reader must understand to read what
//! is on disk under that root:
//!
//! ```json
//! {"floor_schema": 1, "stores": {"aft.db": 13, "search": 4, "semantic": 7}}
//! ```
//!
//! A build can read the root when, for every store in the floor, its own
//! highest readable format version (see `aft --formats`) is at least the
//! floor's value. Format versions are already ordered integers owned by each
//! store, so the comparison needs no release registry and never orders Git
//! hashes or package versions (two different builds can both call themselves
//! the same package version).
//!
//! Rules this module enforces:
//! * The floor is raised *before* the first write of a format that needs it
//!   ([`publish_with_floor`]), so a crash between the two leaves a floor that
//!   is too high (rollback conservatively blocked), never one that is too low.
//! * It is written through a temporary file, fsynced, renamed over the old
//!   file and the directory fsynced, under a cross-process lock.
//! * It is never lowered: raising takes the per-store maximum, and store names
//!   this build does not know are preserved untouched.
//! * A floor this build cannot interpret (a newer `floor_schema`, or a file
//!   that does not parse) is never overwritten; every store is refused.
//!
//! On startup ([`prepare`]) a build whose readable versions are below the
//! floor refuses the affected components by name rather than refusing to start:
//! the floor is per store, so a component whose store is still readable (file
//! edits, outline, reads) keeps working, and each refused component reports
//! the floor as its unavailable reason on the status surfaces.
//!
//! Placement (`scripts/stage-card.sh`) reads the same file and refuses to stage
//! a card whose `aft --formats` is below it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::persisted_format::{self, PersistedStore, UnsupportedPersistedFormat};

/// File name of the floor inside the storage root.
pub const FLOOR_FILE: &str = "reader-floor.json";
/// Layout version of the floor file itself. Readers refuse a higher one.
pub const FLOOR_SCHEMA: u32 = 1;
const LOCK_FILE: &str = "reader-floor.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-store minimum reader format versions, keyed by store name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaderFloor {
    pub stores: BTreeMap<String, u64>,
}

impl ReaderFloor {
    pub fn get(&self, store: PersistedStore) -> Option<u64> {
        self.stores.get(store.name()).copied()
    }

    /// Stores whose floor is above what this build reads.
    pub fn refusals(&self, floor_path: &Path) -> Vec<UnsupportedPersistedFormat> {
        PersistedStore::ALL
            .into_iter()
            .filter_map(|store| {
                let required = self.get(store)?;
                (required > u64::from(store.supported()))
                    .then(|| UnsupportedPersistedFormat::floor(store, floor_path, required))
            })
            .collect()
    }

    /// Store names in the floor that this build does not know at all.
    pub fn unknown_stores(&self) -> Vec<&str> {
        self.stores
            .keys()
            .map(String::as_str)
            .filter(|name| PersistedStore::from_name(name).is_none())
            .collect()
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "floor_schema": FLOOR_SCHEMA,
            "stores": self.stores,
        })
    }
}

#[derive(Debug)]
pub enum FloorError {
    Io {
        path: PathBuf,
        error: io::Error,
    },
    /// The file exists but is not a floor this build can parse.
    Unparseable {
        path: PathBuf,
        detail: String,
    },
    /// The file declares a floor schema newer than [`FLOOR_SCHEMA`].
    NewerSchema {
        path: PathBuf,
        found: u64,
    },
    Lock {
        path: PathBuf,
        detail: String,
    },
}

impl fmt::Display for FloorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FloorError::Io { path, error } => {
                write!(f, "reader floor {}: I/O error: {error}", path.display())
            }
            FloorError::Unparseable { path, detail } => {
                write!(f, "reader floor {} cannot be parsed: {detail}", path.display())
            }
            FloorError::NewerSchema { path, found } => write!(
                f,
                "{}: reader floor {} has floor_schema {found}; this build reads up to {FLOOR_SCHEMA}",
                persisted_format::CODE,
                path.display()
            ),
            FloorError::Lock { path, detail } => {
                write!(f, "reader floor lock {}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for FloorError {}

pub fn floor_path(storage_root: &Path) -> PathBuf {
    storage_root.join(FLOOR_FILE)
}

/// Read the floor without changing anything. `Ok(None)` when the root has no
/// floor yet.
pub fn read(storage_root: &Path) -> Result<Option<ReaderFloor>, FloorError> {
    let path = floor_path(storage_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FloorError::Io { path, error }),
    };
    parse(&path, &bytes).map(Some)
}

fn parse(path: &Path, bytes: &[u8]) -> Result<ReaderFloor, FloorError> {
    #[cfg(test)]
    tests::PARSE_COUNT.with(|count| count.set(count.get() + 1));
    let unparseable = |detail: String| FloorError::Unparseable {
        path: path.to_path_buf(),
        detail,
    };
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| unparseable(error.to_string()))?;
    // Read the schema before anything else, so a newer floor layout is named
    // as newer rather than reported as a shape error.
    let schema = value
        .get("floor_schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| unparseable("missing integer floor_schema".to_string()))?;
    if schema > u64::from(FLOOR_SCHEMA) {
        return Err(FloorError::NewerSchema {
            path: path.to_path_buf(),
            found: schema,
        });
    }
    if schema != u64::from(FLOOR_SCHEMA) {
        return Err(unparseable(format!("unsupported floor_schema {schema}")));
    }
    let stores = value
        .get("stores")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| unparseable("missing stores object".to_string()))?;
    let mut floor = ReaderFloor::default();
    for (name, version) in stores {
        let version = version
            .as_u64()
            .ok_or_else(|| unparseable(format!("store {name} has a non-integer version")))?;
        floor.stores.insert(name.clone(), version);
    }
    Ok(floor)
}

/// Raise the floor so every `(store, version)` requirement is met, keeping
/// the per-store maximum. Never lowers any store and never drops a store name
/// this build does not know. Writes nothing when the floor already covers the
/// requirements. Refuses (without writing) when the existing floor cannot be
/// interpreted. A covered v3 request re-syncs the storage-root directory: a
/// previous raise may have renamed the floor but failed to sync that rename.
/// Covered legacy requests keep their existing no-sync behavior.
pub fn raise(
    storage_root: &Path,
    requirements: &[(PersistedStore, u32)],
) -> Result<ReaderFloor, FloorError> {
    crate::persisted_format::register_storage_root(storage_root);
    crate::production_storage::refuse_write(storage_root).map_err(|error| FloorError::Io {
        path: storage_root.to_path_buf(),
        error,
    })?;
    let path = floor_path(storage_root);
    crate::private_storage::open_root(storage_root).map_err(|error| FloorError::Io {
        path: storage_root.to_path_buf(),
        error,
    })?;
    let lock_path = storage_root.join(LOCK_FILE);
    let _lock = crate::fs_lock::try_acquire(&lock_path, LOCK_TIMEOUT).map_err(|error| {
        FloorError::Lock {
            path: lock_path.clone(),
            detail: error.to_string(),
        }
    })?;
    let current = read(storage_root)?;
    let mut next = current.clone().unwrap_or_default();
    for (store, version) in requirements {
        let entry = next.stores.entry(store.name().to_string()).or_insert(0);
        *entry = (*entry).max(u64::from(*version));
    }
    let strict = requirements.iter().any(|(store, _)| {
        matches!(
            store,
            PersistedStore::ViewsV3Registry | PersistedStore::ViewsV3Store
        )
    });
    if current.as_ref() == Some(&next) {
        if strict {
            sync_dir(storage_root, strict).map_err(|error| FloorError::Io {
                path: storage_root.to_path_buf(),
                error,
            })?;
        }
        return Ok(next);
    }
    write_atomic(&path, &next, strict)?;
    Ok(next)
}

/// Raise the floor for `store` to `version`, then run `write`. The floor is
/// installed before the first byte of the new format exists. Directory sync
/// is required for v3 stores but remains best-effort for legacy stores. If
/// raising fails, `write` does not run.
pub fn publish_with_floor<T>(
    storage_root: &Path,
    store: PersistedStore,
    version: u32,
    write: impl FnOnce() -> T,
) -> Result<T, FloorError> {
    raise(storage_root, &[(store, version)])?;
    Ok(write())
}

/// Raise both v3 floor domains together, then run the publication closure.
/// The caller supplies each store's required format version. Every v3 writer
/// must use this boundary, even for a root whose floor already covers it.
/// Before creating any v3 file, the closure must notify its durability observer
/// at `V3FloorRaised`, the point where both floor domains are durable.
pub fn publish_v3_with_floor<T>(
    storage_root: &Path,
    registry_version: u32,
    store_version: u32,
    write: impl FnOnce() -> T,
) -> Result<T, FloorError> {
    raise(
        storage_root,
        &[
            (PersistedStore::ViewsV3Registry, registry_version),
            (PersistedStore::ViewsV3Store, store_version),
        ],
    )?;
    Ok(write())
}

fn write_atomic(path: &Path, floor: &ReaderFloor, strict: bool) -> Result<(), FloorError> {
    let io_error = |error: io::Error| FloorError::Io {
        path: path.to_path_buf(),
        error,
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{FLOOR_FILE}.tmp.{}.{nanos}", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut file = crate::private_storage::options()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let mut text = serde_json::to_vec_pretty(&floor.to_json()).map_err(io::Error::other)?;
        text.push(b'\n');
        file.write_all(&text)?;
        crate::durability::sync_file(&file, path)?;
        drop(file);
        #[cfg(test)]
        tests::crash_before_rename_hook(&tmp)?;
        fs::rename(&tmp, path)?;
        sync_dir(dir, strict)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(io_error)
}

fn sync_dir(dir: &Path, strict: bool) -> io::Result<()> {
    let result = crate::durability::sync_dir(dir);
    // Legacy floor writers ignore every directory-sync error. Only v3 writers
    // require directory durability before their publication closure may run.
    if strict {
        result
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn prepare_allowed(_storage_root: &Path) -> bool {
    true
}

/// Unit tests drive configure in-process, and some of them never pass a
/// storage directory, which resolves to the operator's live storage root.
/// The floor is only written under the system temp directory in unit tests so
/// a test run can never stamp the live root.
#[cfg(test)]
fn prepare_allowed(storage_root: &Path) -> bool {
    let temp = std::env::temp_dir();
    let canonical_temp = fs::canonicalize(&temp).unwrap_or_else(|_| temp.clone());
    storage_root.starts_with(&temp) || storage_root.starts_with(&canonical_temp)
}

fn prepared_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static PREPARED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    PREPARED.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FloorStamp {
    size: u64,
    modified: SystemTime,
    /// The floor is replaced by renaming a new file over it. A replacement
    /// written within one tick of the filesystem clock keeps the size and
    /// modification time of a same-length floor, and only its identity shows
    /// that the refusals derived from the old floor must be recomputed.
    identity: crate::file_identity::FileIdentity,
}

impl FloorStamp {
    fn new(
        metadata: &fs::Metadata,
        identity: crate::file_identity::FileIdentity,
    ) -> io::Result<Self> {
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified()?,
            identity,
        })
    }
}

fn floor_stamps() -> &'static Mutex<HashMap<PathBuf, Option<FloorStamp>>> {
    static STAMPS: OnceLock<Mutex<HashMap<PathBuf, Option<FloorStamp>>>> = OnceLock::new();
    STAMPS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stat_floor(path: &Path) -> io::Result<Option<FloorStamp>> {
    match crate::file_identity::metadata(path) {
        Ok((metadata, identity)) => FloorStamp::new(&metadata, identity).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_snapshot(path: &Path) -> io::Result<(Option<FloorStamp>, Option<Vec<u8>>)> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => return Err(error),
    };
    // The open handle keeps naming the same file throughout, so one identity
    // read serves both stamps.
    let identity = crate::file_identity::FileIdentity::of_file(&file);
    let before = FloorStamp::new(&file.metadata()?, identity)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = FloorStamp::new(&file.metadata()?, identity)?;
    if before != after || after.size != bytes.len() as u64 {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "reader floor changed while being read",
        ));
    }
    Ok((Some(after), Some(bytes)))
}

/// Recheck a refused root on a persistence retry. The caller must apply its
/// retry backoff before calling; readable roots incur no filesystem work.
pub(crate) fn refresh_refused(storage_root: &Path) -> Result<(), FloorError> {
    let path = floor_path(storage_root);
    if !persisted_format::refusals_under(storage_root)
        .iter()
        .any(|refusal| {
            refusal.source != persisted_format::RefusalSource::Artifact && refusal.path == path
        })
    {
        return Ok(());
    }
    let mut stamps = floor_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lock_path = storage_root.join(LOCK_FILE);
    let _lock = crate::fs_lock::try_acquire(&lock_path, LOCK_TIMEOUT).map_err(|error| {
        FloorError::Lock {
            path: lock_path,
            detail: error.to_string(),
        }
    })?;
    let stamp = stat_floor(&path).map_err(|error| FloorError::Io {
        path: path.clone(),
        error,
    })?;
    if stamps.get(storage_root) == Some(&stamp) {
        return Ok(());
    }
    let (stamp, bytes) = read_snapshot(&path).map_err(|error| FloorError::Io {
        path: path.clone(),
        error,
    })?;
    let floor = bytes.map_or(Ok(None), |bytes| parse(&path, &bytes).map(Some));
    let refusals = floor_refusals(&path, floor)?;
    persisted_format::replace_floor_refusals(storage_root, &refusals);
    stamps.insert(storage_root.to_path_buf(), stamp);
    Ok(())
}

/// Startup check for one storage root, done once per root per process before
/// any artifact under it is read or written.
///
/// Records a floor refusal for every store whose floor is above what this
/// build reads (or for every store when the floor cannot be interpreted), so
/// the owning components refuse by name. Then writes today's formats into the
/// floor as its baseline, so the next format change has a floor to raise.
/// Returns the refusals it recorded.
pub fn prepare(storage_root: &Path) -> Vec<UnsupportedPersistedFormat> {
    crate::persisted_format::register_storage_root(storage_root);
    if !prepare_allowed(storage_root) {
        return Vec::new();
    }
    {
        let mut prepared = prepared_roots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !prepared.insert(storage_root.to_path_buf()) {
            return Vec::new();
        }
    }
    prepare_uncached(storage_root)
}

/// A floor lives in durable storage, and its refusals also cover artifacts on
/// a separate `storage.cache_dir` volume: refusal routing spells paths inside
/// a prepared cache root under the durable root that owns it (see
/// [`crate::root_cache::durable_route`]), so nothing is recorded for the
/// cache root itself, which never becomes a storage owner of its own.
pub(crate) fn prepare_artifact_root(storage: &Path, _artifacts: &Path) {
    prepare(storage);
}

fn prepare_uncached(storage_root: &Path) -> Vec<UnsupportedPersistedFormat> {
    let path = floor_path(storage_root);
    let observed = read_snapshot(&path);
    let floor = match observed {
        Ok((stamp, bytes)) => {
            floor_stamps()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(storage_root.to_path_buf(), stamp);
            bytes.map_or(Ok(None), |bytes| parse(&path, &bytes).map(Some))
        }
        Err(error) => Err(FloorError::Io {
            path: path.clone(),
            error,
        }),
    };
    let refusals = match floor_refusals(&path, floor) {
        Ok(refusals) => refusals,
        Err(error) => {
            // A failed startup read can be retried by the next configure.
            crate::slog_warn!("{error}");
            prepared_roots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(storage_root);
            return Vec::new();
        }
    };
    for refusal in &refusals {
        persisted_format::record(refusal, storage_root);
    }
    let floor_interpretable = !refusals
        .iter()
        .any(|refusal| refusal.source == persisted_format::RefusalSource::FloorUnreadable);
    if floor_interpretable {
        let baseline = PersistedStore::ALL
            .into_iter()
            .filter(|store| {
                !matches!(
                    store,
                    PersistedStore::ViewsV3Registry | PersistedStore::ViewsV3Store
                )
            })
            .map(|store| (store, store.written()))
            .collect::<Vec<_>>();
        if let Err(error) = raise(storage_root, &baseline) {
            crate::slog_warn!("failed to write the reader floor baseline: {error}");
        }
    }
    refusals
}

fn floor_refusals(
    path: &Path,
    floor: Result<Option<ReaderFloor>, FloorError>,
) -> Result<Vec<UnsupportedPersistedFormat>, FloorError> {
    Ok(match floor {
        Ok(floor) => {
            let floor = floor.unwrap_or_default();
            let unknown = floor.unknown_stores();
            if !unknown.is_empty() {
                // This build cannot refuse a store it has never heard of, so
                // it only preserves the entry. scripts/stage-card.sh refuses
                // to stage a build whose `--formats` lacks a store the floor
                // names, which keeps such a build from being deployed.
                crate::slog_warn!(
                    "reader floor {} names stores this build does not know ({}); preserving them",
                    path.display(),
                    unknown.join(", ")
                );
            }
            floor.refusals(path)
        }
        Err(FloorError::NewerSchema { found, .. }) => PersistedStore::ALL
            .into_iter()
            .map(|store| UnsupportedPersistedFormat::floor_unreadable(store, path, found))
            .collect(),
        Err(FloorError::Unparseable { detail, .. }) => {
            crate::slog_warn!("reader floor {} cannot be parsed: {detail}", path.display());
            PersistedStore::ALL
                .into_iter()
                .map(|store| UnsupportedPersistedFormat::floor_unreadable(store, path, 0))
                .collect()
        }
        Err(error) => return Err(error),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    thread_local! {
        pub(super) static PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    #[test]
    fn durability_reader_floor_keeps_two_syncs() {
        let storage = tempfile::tempdir().unwrap();
        crate::durability::take();
        let version = PersistedStore::SemanticIndex.supported();
        raise(storage.path(), &[(PersistedStore::SemanticIndex, version)]).unwrap();
        let events = crate::durability::take();
        assert_eq!(
            crate::durability::sync_count(&events),
            if cfg!(unix) { 2 } else { 1 },
            "{events:?}"
        );
        assert_eq!(
            read(storage.path())
                .unwrap()
                .unwrap()
                .get(PersistedStore::SemanticIndex),
            Some(u64::from(version))
        );
    }

    #[cfg(unix)]
    #[test]
    fn covered_legacy_raise_performs_no_directory_sync() {
        let root = tempfile::tempdir().unwrap();
        let requirements = [(PersistedStore::SearchIndex, 4)];
        raise(root.path(), &requirements).unwrap();
        let before = fs::read(floor_path(root.path())).unwrap();
        crate::durability::take();
        raise(root.path(), &requirements).unwrap();
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
        assert_eq!(fs::read(floor_path(root.path())).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_raise_ignores_eio_directory_sync_failure() {
        let root = tempfile::tempdir().unwrap();
        fail_directory_sync(root.path(), Some(libc::EIO));
        let result = raise(root.path(), &[(PersistedStore::SearchIndex, 4)]);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            read(root.path())
                .unwrap()
                .unwrap()
                .get(PersistedStore::SearchIndex),
            Some(4)
        );
        assert!(crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| slot.borrow().is_none()));
        fail_directory_sync(root.path(), Some(libc::EIO));
        let mut wrote = false;
        publish_with_floor(root.path(), PersistedStore::SearchIndex, 5, || wrote = true).unwrap();
        assert!(wrote);
        assert_eq!(
            read(root.path())
                .unwrap()
                .unwrap()
                .get(PersistedStore::SearchIndex),
            Some(5)
        );
        assert!(crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| slot.borrow().is_none()));
    }

    #[cfg(unix)]
    #[test]
    fn v3_floor_write_propagates_directory_sync_failure() {
        let root = tempfile::tempdir().unwrap();
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            *slot.borrow_mut() = Some(root.path().to_path_buf());
        });
        let mut wrote = false;
        let result = publish_v3_with_floor(root.path(), 1, 1, || {
            wrote = true;
        });
        assert!(matches!(result, Err(FloorError::Io { .. })), "{result:?}");
        assert!(!wrote, "a failed floor sync must prevent publication");
    }

    #[cfg(unix)]
    #[test]
    fn covered_v3_floor_resyncs_without_rewriting_and_propagates_failure() {
        let root = tempfile::tempdir().unwrap();
        let requirements = [(PersistedStore::ViewsV3Store, 1)];
        raise(root.path(), &requirements).unwrap();
        let before = fs::read(floor_path(root.path())).unwrap();
        let stamp = fs::metadata(floor_path(root.path()))
            .unwrap()
            .modified()
            .unwrap();
        crate::durability::take();
        raise(root.path(), &requirements).unwrap();
        assert_eq!(
            crate::durability::take(),
            vec![(
                crate::durability::EventKind::DirectorySync,
                root.path().to_path_buf()
            )]
        );
        assert_eq!(fs::read(floor_path(root.path())).unwrap(), before);
        assert_eq!(
            fs::metadata(floor_path(root.path()))
                .unwrap()
                .modified()
                .unwrap(),
            stamp
        );
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            *slot.borrow_mut() = Some(root.path().to_path_buf());
        });
        let result = raise(root.path(), &requirements);
        assert!(matches!(result, Err(FloorError::Io { .. })), "{result:?}");
    }

    #[test]
    fn uninterpretable_floor_is_untouched_by_raise() {
        for bytes in [b"not json".as_slice(), br#"{"floor_schema":2,"stores":{}}"#] {
            let root = tempfile::tempdir().unwrap();
            fs::write(floor_path(root.path()), bytes).unwrap();
            let result = raise(root.path(), &[(PersistedStore::SearchIndex, 4)]);
            assert!(matches!(
                result,
                Err(FloorError::Unparseable { .. } | FloorError::NewerSchema { .. })
            ));
            assert_eq!(fs::read(floor_path(root.path())).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    fn fail_directory_sync(root: &Path, errno: Option<i32>) {
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            *slot.borrow_mut() = Some(root.to_path_buf());
        });
        crate::durability::FAIL_DIRECTORY_SYNC_ERRNO.with(|slot| slot.set(errno));
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_directory_sync_keeps_old_store_publication_compatible() {
        for errno in [libc::EINVAL, libc::ENOTSUP] {
            let root = tempfile::tempdir().unwrap();
            fail_directory_sync(root.path(), Some(errno));
            assert!(prepare_uncached(root.path()).is_empty());
            assert!(read(root.path()).unwrap().is_some());
            assert!(crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| slot.borrow().is_none()));
            assert_eq!(
                crate::durability::FAIL_DIRECTORY_SYNC_ERRNO.with(|slot| slot.get()),
                None
            );
            let next = PersistedStore::SearchIndex.written() + 1;
            fail_directory_sync(root.path(), Some(errno));
            let mut wrote = false;
            publish_with_floor(root.path(), PersistedStore::SearchIndex, next, || {
                wrote = true
            })
            .unwrap();
            assert!(wrote);
            assert_eq!(
                read(root.path())
                    .unwrap()
                    .unwrap()
                    .get(PersistedStore::SearchIndex),
                Some(u64::from(next))
            );
            assert!(crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| slot.borrow().is_none()));
            assert_eq!(
                crate::durability::FAIL_DIRECTORY_SYNC_ERRNO.with(|slot| slot.get()),
                None
            );
        }
    }
    use std::cell::RefCell;

    thread_local! {
        static CRASH_BEFORE_RENAME: RefCell<bool> = const { RefCell::new(false) };
    }

    /// Simulate a crash after the temporary floor is written and synced but
    /// before it is renamed into place.
    pub(super) fn crash_before_rename_hook(_tmp: &Path) -> io::Result<()> {
        if CRASH_BEFORE_RENAME.with(|flag| *flag.borrow()) {
            return Err(io::Error::other("simulated crash before floor rename"));
        }
        Ok(())
    }

    fn current_versions() -> BTreeMap<String, u64> {
        PersistedStore::ALL
            .into_iter()
            .map(|store| (store.name().to_string(), u64::from(store.written())))
            .collect()
    }

    fn write_floor(root: &Path, json: serde_json::Value) {
        fs::write(floor_path(root), serde_json::to_vec(&json).unwrap()).unwrap();
    }

    #[test]
    fn first_prepare_writes_todays_formats_as_the_baseline() {
        let root = tempfile::tempdir().unwrap();
        assert!(prepare_uncached(root.path()).is_empty());
        let floor = read(root.path()).unwrap().expect("baseline floor written");
        let mut expected = current_versions();
        expected.remove("views_v3_registry");
        expected.remove("views_v3_store");
        assert_eq!(floor.stores, expected);
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(floor_path(root.path())).unwrap()).unwrap();
        assert_eq!(raw["floor_schema"], FLOOR_SCHEMA);
    }

    #[test]
    fn v3_registration_is_advertised_but_not_prepared() {
        let formats = persisted_format::formats_json();
        // The registry reads verification format/schema version 3; plane stores
        // keep format 1. Neither appears in the startup baseline floor.
        for (name, version) in [("views_v3_registry", 3), ("views_v3_store", 1)] {
            assert_eq!(formats["stores"][name], version, "{name}");
            let store = PersistedStore::from_name(name).expect("registered store");
            assert!(PersistedStore::ALL.contains(&store));
            assert_eq!(store.supported(), version);
            assert_eq!(store.written(), version);
        }
        let root = tempfile::tempdir().unwrap();
        assert!(prepare_uncached(root.path()).is_empty());
        let floor = read(root.path()).unwrap().unwrap();
        assert!(!floor.stores.contains_key("views_v3_registry"));
        assert!(!floor.stores.contains_key("views_v3_store"));
        let before = fs::read(floor_path(root.path())).unwrap();
        assert!(prepare_uncached(root.path()).is_empty());
        assert_eq!(fs::read(floor_path(root.path())).unwrap(), before);
        assert!(!root.path().join("blobs/v3").exists());
    }

    #[test]
    fn v3_store_above_supported_is_a_named_floor_refusal() {
        let root = tempfile::tempdir().unwrap();
        write_floor(
            root.path(),
            serde_json::json!({
                "floor_schema": 1, "stores": {"views_v3_store": 2}
            }),
        );
        let refusals = prepare_uncached(root.path());
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        assert_eq!(refusals[0].store.name(), "views_v3_store");
        assert_eq!(refusals[0].source, persisted_format::RefusalSource::Floor);
        assert_eq!(refusals[0].found, 2);
        assert!(refusals[0].to_string().contains("views_v3_store"));
        assert_eq!(
            read(root.path()).unwrap().unwrap().stores["views_v3_store"],
            2
        );
    }

    #[test]
    fn v3_publication_raises_both_domains_once_before_any_v3_file() {
        let root = tempfile::tempdir().unwrap();
        assert!(prepare_uncached(root.path()).is_empty());
        let mut expected = read(root.path()).unwrap().unwrap().stores;
        expected.insert("views_v3_registry".into(), 1);
        expected.insert("views_v3_store".into(), 1);
        crate::durability::take();
        let value = publish_v3_with_floor(root.path(), 1, 1, || {
            assert!(!root.path().join("blobs/v3").exists());
            assert_eq!(read(root.path()).unwrap().unwrap().stores, expected);
            42
        })
        .unwrap();
        assert_eq!(value, 42);
        let events = crate::durability::take();
        assert_eq!(
            events
                .iter()
                .filter(|(kind, path)| {
                    *kind == crate::durability::EventKind::FileSync
                        && *path == floor_path(root.path())
                })
                .count(),
            1,
            "both requirements must be raised in one atomic write: {events:?}"
        );
    }

    #[test]
    fn v3_publication_uses_both_supplied_versions_and_preserves_other_stores() {
        let root = tempfile::tempdir().unwrap();
        write_floor(
            root.path(),
            serde_json::json!({
                "floor_schema": 1,
                "stores": {"views_v3_registry": 3, "views_v3_store": 1, "future_store": 8}
            }),
        );
        publish_v3_with_floor(root.path(), 2, 4, || {
            assert_eq!(
                read(root.path()).unwrap().unwrap().stores,
                BTreeMap::from([
                    ("views_v3_registry".into(), 3),
                    ("views_v3_store".into(), 4),
                    ("future_store".into(), 8)
                ])
            );
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn v3_failed_sync_skips_closure_and_covered_retry_resyncs() {
        let root = tempfile::tempdir().unwrap();
        let mut writes = 0;
        for _ in 0..2 {
            fail_directory_sync(root.path(), None);
            let result = publish_v3_with_floor(root.path(), 1, 1, || writes += 1);
            assert!(matches!(result, Err(FloorError::Io { .. })), "{result:?}");
            assert_eq!(writes, 0);
            assert!(!root.path().join("blobs/v3").exists());
            let floor = read(root.path()).unwrap().unwrap();
            assert_eq!(
                floor.stores,
                BTreeMap::from([
                    ("views_v3_registry".into(), 1),
                    ("views_v3_store".into(), 1)
                ])
            );
        }
        let before = fs::read(floor_path(root.path())).unwrap();
        crate::durability::take();
        publish_v3_with_floor(root.path(), 1, 1, || {
            assert!(!root.path().join("blobs/v3").exists());
            writes += 1;
        })
        .unwrap();
        assert_eq!(writes, 1);
        assert_eq!(fs::read(floor_path(root.path())).unwrap(), before);
        assert_eq!(
            crate::durability::take(),
            vec![(
                crate::durability::EventKind::DirectorySync,
                root.path().to_path_buf()
            )]
        );
    }

    #[cfg(unix)]
    #[test]
    fn v3_unsupported_directory_sync_is_strict_for_either_domain() {
        for errno in [libc::EINVAL, libc::ENOTSUP] {
            let root = tempfile::tempdir().unwrap();
            for _ in 0..2 {
                fail_directory_sync(root.path(), Some(errno));
                let mut wrote = false;
                let result = publish_v3_with_floor(root.path(), 1, 1, || wrote = true);
                assert!(
                    matches!(result, Err(FloorError::Io { ref error, .. }) if error.raw_os_error() == Some(errno)),
                    "{result:?}"
                );
                assert!(!wrote);
                assert!(!root.path().join("blobs/v3").exists());
            }
            for store in [
                PersistedStore::ViewsV3Registry,
                PersistedStore::ViewsV3Store,
            ] {
                fail_directory_sync(root.path(), Some(errno));
                let result = raise(root.path(), &[(store, 2)]);
                assert!(
                    matches!(result, Err(FloorError::Io { ref error, .. }) if error.raw_os_error() == Some(errno)),
                    "{result:?}"
                );
            }
        }
    }

    #[test]
    fn raise_never_lowers_and_preserves_unknown_stores() {
        let root = tempfile::tempdir().unwrap();
        write_floor(
            root.path(),
            serde_json::json!({"floor_schema": 1, "stores": {"search": 9, "semantic": 2, "future_store": 3}}),
        );
        let floor = raise(
            root.path(),
            &[
                (PersistedStore::SearchIndex, 4),
                (PersistedStore::SemanticIndex, 5),
            ],
        )
        .unwrap();
        assert_eq!(
            floor.stores["search"], 9,
            "a lower requirement must not lower the floor"
        );
        assert_eq!(
            floor.stores["future_store"], 3,
            "unknown stores must be preserved"
        );
        assert_eq!(floor.stores["semantic"], 5);
        let floor = raise(root.path(), &[(PersistedStore::SearchIndex, 11)]).unwrap();
        assert_eq!(floor.stores["search"], 11);
        assert_eq!(floor.stores["future_store"], 3);
        assert_eq!(floor.stores["semantic"], 5);
        assert_eq!(read(root.path()).unwrap().unwrap().stores["search"], 11);
    }

    #[test]
    fn floor_is_raised_before_the_new_format_write_runs() {
        let root = tempfile::tempdir().unwrap();
        prepare_uncached(root.path());
        let next = PersistedStore::SemanticIndex.written() + 1;
        let seen_during_write =
            publish_with_floor(root.path(), PersistedStore::SemanticIndex, next, || {
                read(root.path())
                    .unwrap()
                    .unwrap()
                    .get(PersistedStore::SemanticIndex)
            })
            .unwrap();
        assert_eq!(seen_during_write, Some(u64::from(next)));
    }

    #[test]
    fn failed_floor_raise_skips_the_write() {
        let root = tempfile::tempdir().unwrap();
        write_floor(
            root.path(),
            serde_json::json!({"floor_schema": 2, "stores": {}}),
        );
        let mut wrote = false;
        let result = publish_with_floor(root.path(), PersistedStore::SearchIndex, 4, || {
            wrote = true;
        });
        assert!(matches!(
            result,
            Err(FloorError::NewerSchema { found: 2, .. })
        ));
        assert!(
            !wrote,
            "the new-format write must not run without a raised floor"
        );
    }

    #[test]
    fn crash_before_rename_leaves_the_previous_floor_intact() {
        let root = tempfile::tempdir().unwrap();
        prepare_uncached(root.path());
        let before = fs::read(floor_path(root.path())).unwrap();
        CRASH_BEFORE_RENAME.with(|flag| *flag.borrow_mut() = true);
        let result = raise(root.path(), &[(PersistedStore::SearchIndex, 50)]);
        CRASH_BEFORE_RENAME.with(|flag| *flag.borrow_mut() = false);
        assert!(result.is_err());
        assert_eq!(fs::read(floor_path(root.path())).unwrap(), before);
        let leftovers = fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(
            leftovers, 0,
            "a failed raise must not leave its temp file behind"
        );
    }

    #[test]
    fn a_floor_above_this_build_refuses_that_store_by_name() {
        let root = tempfile::tempdir().unwrap();
        let above = u64::from(PersistedStore::CallgraphStore.supported()) + 1;
        write_floor(
            root.path(),
            serde_json::json!({"floor_schema": 1, "stores": {"callgraph": above}}),
        );
        let before = fs::read(floor_path(root.path())).unwrap();
        let refusals = prepare_uncached(root.path());
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        assert_eq!(refusals[0].store, PersistedStore::CallgraphStore);
        assert!(refusals[0].to_string().contains(persisted_format::CODE));
        let artifact = root.path().join("callgraph").join("k");
        assert_eq!(
            persisted_format::refusal_covering(PersistedStore::CallgraphStore, &artifact),
            Some(refusals[0].clone())
        );
        assert!(
            persisted_format::refusal_covering(PersistedStore::SearchIndex, &artifact).is_none(),
            "stores the floor still allows keep working"
        );
        let after = read(root.path()).unwrap().unwrap();
        assert_eq!(
            after.stores["callgraph"], above,
            "the floor is never lowered"
        );
        assert_ne!(
            fs::read(floor_path(root.path())).unwrap(),
            before,
            "baseline added"
        );
    }

    #[test]
    fn a_newer_floor_schema_refuses_every_store_and_is_not_rewritten() {
        let root = tempfile::tempdir().unwrap();
        write_floor(
            root.path(),
            serde_json::json!({"floor_schema": 7, "stores": {}}),
        );
        let before = fs::read(floor_path(root.path())).unwrap();
        let refusals = prepare_uncached(root.path());
        assert_eq!(refusals.len(), PersistedStore::ALL.len());
        assert_eq!(fs::read(floor_path(root.path())).unwrap(), before);
    }

    fn prepared_refused_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let mut stores = current_versions();
        stores.insert(
            "aft.db".into(),
            u64::from(PersistedStore::AftDb.supported()) + 1,
        );
        write_floor(
            root.path(),
            serde_json::json!({"floor_schema": FLOOR_SCHEMA, "stores": stores}),
        );
        assert_eq!(prepare(root.path()).len(), 1);
        root
    }

    #[test]
    fn refused_retry_stats_unchanged_floor_without_reparsing() {
        let root = prepared_refused_root();
        PARSE_COUNT.with(|count| count.set(0));
        refresh_refused(root.path()).unwrap();
        refresh_refused(root.path()).unwrap();
        assert_eq!(PARSE_COUNT.with(|count| count.get()), 0);
        assert!(persisted_format::refusal_covering(
            PersistedStore::AftDb,
            &root.path().join("aft.db")
        )
        .is_some());
    }

    #[test]
    fn refused_retry_reparses_on_mtime_change_and_preserves_artifact_refusal() {
        let root = prepared_refused_root();
        let path = floor_path(root.path());
        let before = fs::metadata(&path).unwrap();
        let mut floor = read(root.path()).unwrap().unwrap();
        floor.stores.insert(
            "aft.db".into(),
            u64::from(PersistedStore::AftDb.supported()),
        );
        let artifact = root.path().join("index/k/cache.bin");
        let artifact_refusal =
            UnsupportedPersistedFormat::artifact(PersistedStore::SearchIndex, &artifact, 99);
        persisted_format::record(&artifact_refusal, &artifact);
        write_floor(root.path(), floor.to_json());
        assert_eq!(before.len(), fs::metadata(&path).unwrap().len());
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_system_time(
                before.modified().unwrap() + Duration::from_secs(1),
            ),
        )
        .unwrap();
        PARSE_COUNT.with(|count| count.set(0));
        refresh_refused(root.path()).unwrap();
        assert_eq!(PARSE_COUNT.with(|count| count.get()), 1);
        assert!(persisted_format::gate(
            PersistedStore::AftDb,
            &root.path().join("aft.db"),
            &root.path().join("aft.db"),
            None
        )
        .is_ok());
        assert_eq!(
            persisted_format::refusal_covering(PersistedStore::SearchIndex, &artifact),
            Some(artifact_refusal)
        );
        assert_eq!(
            read(root.path()).unwrap().unwrap(),
            floor,
            "refresh must not write the floor"
        );
    }

    #[test]
    fn refused_retry_reparses_on_size_change_and_caches_newer_schema() {
        let root = prepared_refused_root();
        let path = floor_path(root.path());
        let before = fs::metadata(&path).unwrap();
        write_floor(
            root.path(),
            serde_json::json!({"floor_schema": FLOOR_SCHEMA + 1, "stores": {}}),
        );
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_system_time(before.modified().unwrap()),
        )
        .unwrap();
        assert_ne!(before.len(), fs::metadata(&path).unwrap().len());
        let bytes = fs::read(&path).unwrap();
        PARSE_COUNT.with(|count| count.set(0));
        refresh_refused(root.path()).unwrap();
        refresh_refused(root.path()).unwrap();
        assert_eq!(PARSE_COUNT.with(|count| count.get()), 1);
        let refusals = persisted_format::refusals_under(root.path());
        assert_eq!(refusals.len(), PersistedStore::ALL.len());
        assert!(refusals.iter().all(|refusal| refusal.source
            == persisted_format::RefusalSource::FloorUnreadable
            && refusal.found == u64::from(FLOOR_SCHEMA + 1)));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn refused_retry_retains_evidence_when_floor_cannot_be_read_safely() {
        let root = prepared_refused_root();
        let path = floor_path(root.path());
        let before = persisted_format::refusals_under(root.path());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(refresh_refused(root.path()).is_err());
        assert_eq!(persisted_format::refusals_under(root.path()), before);
    }

    #[test]
    fn refused_retry_reparses_a_same_length_replacement_with_identical_times() {
        // A floor rewritten by atomic replacement within one clock tick keeps
        // its size and times; only the file identity shows the new file.
        let root = prepared_refused_root();
        let path = floor_path(root.path());
        let supported = PersistedStore::AftDb.supported();
        let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        let store = text.find("\"aft.db\"").unwrap();
        let (head, tail) = text.split_at(store);
        let lowered = format!(
            "{head}{}",
            tail.replacen(&(supported + 1).to_string(), &supported.to_string(), 1)
        );
        crate::file_identity::replace_keeping_times(&path, lowered.as_bytes());
        assert_eq!(
            read(root.path()).unwrap().unwrap().stores["aft.db"],
            u64::from(supported)
        );
        refresh_refused(root.path()).unwrap();
        assert!(
            persisted_format::gate(
                PersistedStore::AftDb,
                &root.path().join("aft.db"),
                &root.path().join("aft.db"),
                None
            )
            .is_ok(),
            "the lowered replacement floor must lift the refusal"
        );
    }

    #[test]
    fn readable_root_is_not_rechecked_on_retry() {
        let root = tempfile::tempdir().unwrap();
        assert!(prepare(root.path()).is_empty());
        fs::write(floor_path(root.path()), b"unparseable").unwrap();
        PARSE_COUNT.with(|count| count.set(0));
        refresh_refused(root.path()).unwrap();
        assert_eq!(PARSE_COUNT.with(|count| count.get()), 0);
        assert!(persisted_format::refusals_under(root.path()).is_empty());
    }
}
