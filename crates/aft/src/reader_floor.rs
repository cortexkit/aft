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

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

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
/// interpreted.
pub fn raise(
    storage_root: &Path,
    requirements: &[(PersistedStore, u32)],
) -> Result<ReaderFloor, FloorError> {
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
    if current.as_ref() == Some(&next) {
        return Ok(next);
    }
    write_atomic(&path, &next)?;
    Ok(next)
}

/// Raise the floor for `store` to `version`, then run `write`. The floor is
/// durable before the first byte of the new format exists, so a crash after
/// the raise leaves the floor ahead of the data, never behind it. If raising
/// fails, `write` does not run.
pub fn publish_with_floor<T>(
    storage_root: &Path,
    store: PersistedStore,
    version: u32,
    write: impl FnOnce() -> T,
) -> Result<T, FloorError> {
    raise(storage_root, &[(store, version)])?;
    Ok(write())
}

fn write_atomic(path: &Path, floor: &ReaderFloor) -> Result<(), FloorError> {
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
        sync_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(io_error)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    let _ = crate::durability::sync_dir(dir);
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

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

/// Startup check for one storage root, done once per root per process before
/// any artifact under it is read or written.
///
/// Records a floor refusal for every store whose floor is above what this
/// build reads (or for every store when the floor cannot be interpreted), so
/// the owning components refuse by name. Then writes today's formats into the
/// floor as its baseline, so the next format change has a floor to raise.
/// Returns the refusals it recorded.
pub fn prepare(storage_root: &Path) -> Vec<UnsupportedPersistedFormat> {
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

fn prepare_uncached(storage_root: &Path) -> Vec<UnsupportedPersistedFormat> {
    let path = floor_path(storage_root);
    let refusals = match read(storage_root) {
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
            floor.refusals(&path)
        }
        Err(FloorError::NewerSchema { found, .. }) => PersistedStore::ALL
            .into_iter()
            .map(|store| UnsupportedPersistedFormat::floor_unreadable(store, &path, found))
            .collect(),
        Err(FloorError::Unparseable { detail, .. }) => {
            crate::slog_warn!("reader floor {} cannot be parsed: {detail}", path.display());
            PersistedStore::ALL
                .into_iter()
                .map(|store| UnsupportedPersistedFormat::floor_unreadable(store, &path, 0))
                .collect()
        }
        Err(error) => {
            // When the floor file cannot be read at all (an I/O error, not a
            // parse error), every artifact under the root fails on its own
            // too, so the floor has nothing to add. Forget this root so the
            // next configure checks it again.
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
            .map(|store| (store, store.written()))
            .collect::<Vec<_>>();
        if let Err(error) = raise(storage_root, &baseline) {
            crate::slog_warn!("failed to write the reader floor baseline: {error}");
        }
    }
    refusals
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
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
        assert_eq!(floor.stores, current_versions());
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(floor_path(root.path())).unwrap()).unwrap();
        assert_eq!(raw["floor_schema"], FLOOR_SCHEMA);
    }

    #[test]
    fn raise_never_lowers_and_preserves_unknown_stores() {
        let root = tempfile::tempdir().unwrap();
        write_floor(
            root.path(),
            serde_json::json!({"floor_schema": 1, "stores": {"search": 9, "future_store": 3}}),
        );
        let floor = raise(root.path(), &[(PersistedStore::SearchIndex, 4)]).unwrap();
        assert_eq!(
            floor.stores["search"], 9,
            "a lower requirement must not lower the floor"
        );
        assert_eq!(
            floor.stores["future_store"], 3,
            "unknown stores must be preserved"
        );
        let floor = raise(root.path(), &[(PersistedStore::SearchIndex, 11)]).unwrap();
        assert_eq!(floor.stores["search"], 11);
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
}
