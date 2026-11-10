//! Explicit durable/artifact locations and conservative cache-volume repair.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

pub(crate) mod markers;
use crate::bash_background::persistence::{PinnedDir, RepairBudget};

#[cfg(test)]
#[path = "cache_location_tests.rs"]
mod tests;
use serde::{Deserialize, Serialize};

pub const CACHE_DIR_UNAVAILABLE: &str = "cache_dir_unavailable";
pub const CACHE_DIR_BUSY: &str = "cache_dir_busy";
/// What agents read when a project's cache folder was contended at setup.
/// The project keeps working and AFT retries with backoff while it stays bound.
pub const CACHE_DIR_BUSY_MESSAGE: &str =
    "cache folder busy: another project was creating it at the same moment; AFT retries automatically";
pub(crate) fn cache_status_code(reason: &str) -> &'static str {
    if reason.starts_with(CACHE_DIR_BUSY) {
        CACHE_DIR_BUSY
    } else {
        CACHE_DIR_UNAVAILABLE
    }
}
pub const CACHE_DIR_ON_STORAGE_DEVICE: &str = "cache_dir_on_storage_device";
/// `storage.cache_dir` contains the durable storage root or sits inside it.
pub const CACHE_DIR_OVERLAPS_STORAGE: &str = "cache_dir_overlaps_storage";
pub(crate) fn unavailable_code(reason: &str) -> &'static str {
    if reason.starts_with(CACHE_DIR_BUSY) {
        CACHE_DIR_BUSY
    } else if reason.starts_with(CACHE_DIR_ON_STORAGE_DEVICE) {
        CACHE_DIR_ON_STORAGE_DEVICE
    } else if reason.starts_with(CACHE_DIR_OVERLAPS_STORAGE) {
        CACHE_DIR_OVERLAPS_STORAGE
    } else {
        CACHE_DIR_UNAVAILABLE
    }
}

#[cfg(test)]
pub(crate) fn prepare_cache_root_for_test(
    config: &mut crate::config::Config,
) -> Option<crate::config_resolve::ConfigWarning> {
    let _volume = config
        .cache_dir
        .as_ref()
        .filter(|parent| parent.is_dir())
        .map(|parent| crate::walk_boundary::simulated_volumes::simulate(parent, u64::MAX - 17));
    prepare_cache_root(config)
}
#[derive(Debug)]
struct ForeignCacheOwner;
impl std::fmt::Display for ForeignCacheOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("artifact root belongs to another storage root")
    }
}
impl std::error::Error for ForeignCacheOwner {}
fn foreign_owner_error() -> io::Error {
    io::Error::other(ForeignCacheOwner)
}
/// `storage.cache_dir` and the durable storage root overlap: the cache
/// folder contains durable storage or sits inside it. A configuration error,
/// never recorded as an unavailable cache root: with cache `/data` and storage
/// `/data/aft` the cache root would be the storage root itself, and recording
/// it would refuse every durable write.
#[derive(Debug)]
struct StorageOverlap(String);
impl std::fmt::Display for StorageOverlap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for StorageOverlap {}
fn storage_overlap_error(cache_dir: &Path, storage: &Path) -> io::Error {
    io::Error::other(StorageOverlap(format!(
        "{CACHE_DIR_OVERLAPS_STORAGE}: storage.cache_dir {} overlaps the storage root {}; the cache folder must neither contain durable storage nor sit inside it",
        cache_dir.display(),
        storage.display()
    )))
}
/// Whether `a` and `b` are the same folder or one contains the other, under
/// any of their spellings (see [`path_spellings`]).
fn paths_overlap(a: &Path, b: &Path) -> bool {
    let b_spellings = path_spellings(b);
    path_spellings(a).iter().any(|a| {
        b_spellings
            .iter()
            .any(|b| a.starts_with(b) || b.starts_with(a))
    })
}
const OWNER: &str = ".aft-owner.json";
const IN_USE: &str = ".aft-in-use.lock";
const DAEMON_USE: &str = "daemon-use.lock";
static EPOCH: AtomicU64 = AtomicU64::new(1);
#[derive(Default)]
struct AccessRegistry {
    admitted: HashMap<Identity, (PathBuf, Option<String>, u64)>,
    owners: HashMap<Identity, PathBuf>,
    // Pathnames can only deny access (missing/replaced/unprepared volumes).
    // Successful admission always requires an open directory's identity.
    locations: HashMap<PathBuf, (Option<String>, u64, Option<Identity>)>,
    // Durable roots configured with a separate `storage.cache_dir`, recorded
    // whether or not that cache root is currently available. New rebuildable
    // cache folders must never appear under these roots.
    split_durable: HashSet<PathBuf>,
    // Cache roots refused because they overlap durable storage. They are
    // never given a location entry, not even the "not prepared" placeholder.
    storage_overlaps: HashSet<PathBuf>,
}
static STATUS: OnceLock<Mutex<AccessRegistry>> = OnceLock::new();
#[cfg(test)]
thread_local! { static BEFORE_REPAIR_REMOVE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static BEFORE_ENROLLMENT_DEVICE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static BEFORE_OTHER_ROOT_OVERLAP: std::cell::RefCell<Option<(PathBuf, Box<dyn FnOnce()>)>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static AFTER_CACHE_ROOT_CREATE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static BEFORE_ROOT_HISTORY_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static AFTER_CACHE_ROOT_RENAME: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }

/// Both roots travel together whenever an operation mixes artifacts and metadata.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StoragePaths {
    pub durable: PathBuf,
    pub artifacts: PathBuf,
}
impl StoragePaths {
    pub fn new(durable: PathBuf, artifacts: PathBuf) -> Self {
        Self { durable, artifacts }
    }
    pub fn same(root: &Path) -> Self {
        Self::new(root.to_path_buf(), root.to_path_buf())
    }
    pub fn for_config(config: &crate::config::Config) -> Self {
        Self::new(
            crate::storage_path::storage_dir(config.storage_dir.as_deref()),
            cache_root(config),
        )
    }
}

/// The shared filesystem lock is released only after the last context/config
/// snapshot using this root is dropped, including snapshots held by workers.
#[derive(Debug)]
pub struct CacheLease {
    _file: File,
    root: PinnedDir,
    root_identity: Identity,
}

/// Ownership of durable storage retained before cache openers run. Keeping the
/// root open prevents its identity from being recycled for an unrelated directory.
#[derive(Debug)]
pub struct ProcessStorageLease {
    _file: File,
    _root: PinnedDir,
    identity: Identity,
}
static PROCESS_STORAGE_LEASES: OnceLock<Mutex<HashMap<Identity, Weak<ProcessStorageLease>>>> =
    OnceLock::new();

#[derive(Debug)]
pub(crate) struct SpeechServingLease {
    _file: File,
    _root: PinnedDir,
    _storage_use: Arc<ProcessStorageLease>,
    identity: Identity,
}

// Ordinary guard drop is not proof that detached speech work has stopped.
// Managed runtimes explicitly remove their matching anchor after quiescence;
// every other owner remains pinned until process exit.
static SPEECH_SERVING_LEASES: OnceLock<Mutex<HashMap<Identity, Arc<SpeechServingLease>>>> =
    OnceLock::new();
impl CacheLease {
    pub(crate) fn same_root(&self, other: &Self) -> bool {
        self.root_identity == other.root_identity
    }
    pub(crate) fn root_key(&self) -> (u64, u64) {
        (self.root_identity.device, self.root_identity.inode)
    }
}
impl PartialEq for CacheLease {
    fn eq(&self, other: &Self) -> bool {
        self.same_root(other)
    }
}
impl Eq for CacheLease {}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
}
fn identity(path: &Path) -> io::Result<Identity> {
    #[cfg(test)]
    if let Some((device, inode)) = crate::bash_background::persistence::REPAIR_IDENTITIES
        .with(|ids| ids.borrow().get(path).copied())
    {
        return Ok(Identity { device, inode });
    }
    crate::config_live::dir_identity(path)
        .map(|(device, inode)| Identity { device, inode })
        .ok_or_else(|| io::Error::other(format!("cannot identify directory {}", path.display())))
}
fn real_directory(path: &Path) -> io::Result<Identity> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other("root is a symlink"));
    }
    if !metadata.is_dir() {
        return Err(io::Error::other("root is not a directory"));
    }
    identity(path)
}

#[derive(Serialize, Deserialize)]
struct ParentIdentity {
    path: PathBuf,
    identity: Identity,
    /// The spelling the user configured, when it differs from `path` because
    /// it runs through a symlink. It must keep naming the same directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    configured: Option<PathBuf>,
    #[serde(default)]
    volume_uuid: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct ParentState {
    version: u32,
    parents: Vec<ParentIdentity>,
}
fn parent_state_path(storage: &Path) -> PathBuf {
    storage.join("retention/cache-parents.json")
}
fn read_parent_state(storage: &Path) -> io::Result<ParentState> {
    match fs::read(parent_state_path(storage)) {
        Ok(bytes) => {
            let state: ParentState = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if state.version != 1 {
                return Err(io::Error::other("unsupported cache parent identity state"));
            }
            Ok(state)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ParentState {
            version: 1,
            parents: Vec::new(),
        }),
        Err(error) => Err(error),
    }
}
fn validate_saved_parent(
    storage: &Path,
    configured: &Path,
    parent: &Path,
    actual: Identity,
) -> io::Result<()> {
    if read_parent_state(storage)?.parents.iter().any(|saved| {
        ((saved.path == parent
            || saved.path == configured
            || saved.configured.as_deref() == Some(configured))
            && saved.identity != actual)
            || saved.identity == actual
                && saved.volume_uuid != crate::volume::cache_volume_uuid(parent)
    }) {
        return Err(io::Error::other("cache_dir is not the volume it was"));
    }
    Ok(())
}
fn remember_parent(
    storage: &Path,
    configured: &Path,
    parent: &Path,
    actual: Identity,
) -> io::Result<()> {
    let _lock = crate::fs_lock::try_acquire(
        &storage.join("retention/cache-parents.lock"),
        Duration::from_secs(5),
    )
    .map_err(io::Error::other)?;
    validate_saved_parent(storage, configured, parent, actual)?;
    let mut state = read_parent_state(storage)?;
    let configured = (configured != parent).then(|| configured.to_path_buf());
    let known = state.parents.iter().any(|saved| {
        saved.identity == actual && (configured.is_none() || saved.configured == configured)
    });
    if !known {
        state.parents.push(ParentIdentity {
            path: parent.to_path_buf(),
            identity: actual,
            configured,
            volume_uuid: crate::volume::cache_volume_uuid(parent),
        });
        crate::durability::atomic_write(
            &parent_state_path(storage),
            &serde_json::to_vec(&state).map_err(io::Error::other)?,
            crate::durability::AtomicWriteDurability::FileAndDirectory,
        )?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn verify_mounted_parent_under(
    parent: &Path,
    actual: Identity,
    volumes: &Path,
    system_devices: &[u64],
) -> io::Result<()> {
    let volumes_id = real_directory(volumes)?;
    if actual == volumes_id {
        return Err(io::Error::other("cache_dir cannot be /Volumes"));
    }
    let mut directory = parent;
    while let Some(ancestor) = directory.parent() {
        if identity(ancestor)? == volumes_id {
            if system_devices.contains(&actual.device) {
                return Err(io::Error::other(format!(
                    "volume {} is not mounted",
                    directory.file_name().unwrap_or_default().to_string_lossy()
                )));
            }
            break;
        }
        directory = ancestor;
    }
    Ok(())
}
#[cfg(test)]
thread_local! { static MOUNT_TEST_SEAM: std::cell::RefCell<Option<(PathBuf, Vec<u64>)>> = const { std::cell::RefCell::new(None) }; }
fn check_parent_mount(parent: &Path, actual: Identity) -> io::Result<()> {
    #[cfg(test)]
    if let Some((volumes, devices)) = MOUNT_TEST_SEAM.with(|seam| seam.borrow().clone()) {
        return verify_mounted_parent_under(parent, actual, &volumes, &devices);
    }
    #[cfg(target_os = "macos")]
    {
        let mut devices = vec![identity(Path::new("/"))?.device];
        let data = Path::new("/System/Volumes/Data");
        if data.try_exists()? {
            devices.push(identity(data)?.device);
        }
        verify_mounted_parent_under(parent, actual, Path::new("/Volumes"), &devices)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (parent, actual);
        Ok(())
    }
}

pub fn cache_root(config: &crate::config::Config) -> PathBuf {
    let root = config
        .cache_dir
        .as_ref()
        .map(|root| root.join("aft"))
        .unwrap_or_else(|| crate::storage_path::storage_dir(config.storage_dir.as_deref()));
    if config.cache_dir.is_some() {
        crate::project_content::register_storage_root(&root);
        // A root that is, contains or sits inside durable storage never gets a
        // location entry: an entry denies every path under it, durable
        // writes included.
        let storage = crate::storage_path::storage_dir(config.storage_dir.as_deref());
        let mut registry = STATUS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if registry.storage_overlaps.contains(&root)
            || root.starts_with(&storage)
            || storage.starts_with(&root)
        {
            return root;
        }
        registry.locations.entry(root.clone()).or_insert_with(|| {
            (
                Some("configured cache root has not been prepared".into()),
                0,
                None,
            )
        });
    }
    root
}
pub fn cache_unavailable(config: &crate::config::Config) -> Option<String> {
    cache_unavailable_for_root(config, &cache_root(config))
}
/// [`cache_unavailable`] against an already-resolved cache root. The health
/// rollup resolves each distinct root once per tick and reuses it here so a
/// many-context tick does not re-resolve the same root once per context.
pub fn cache_unavailable_for_root(config: &crate::config::Config, root: &Path) -> Option<String> {
    if let Some(reason) = &config.cache_config_unknown {
        return Some(format!("cache_dir_config_unknown: {reason}"));
    }
    if let Some(reason) = &config.cache_unavailable {
        return Some(reason.clone());
    }
    config.cache_dir.as_ref()?;
    if config.cache_epoch == 0 || config.cache_lease.is_none() {
        return Some("configured cache root has not been prepared".into());
    }
    config.cache_unavailable.clone().or_else(|| {
        let (reason, epoch, _) = status(root);
        reason.or_else(|| {
            (epoch != config.cache_epoch).then(|| "cache root transition pending configure".into())
        })
    })
}
/// The storage root whose identity records key the artifacts kept under
/// `storage`.
///
/// A cache root prepared for `storage.cache_dir` holds rebuildable artifacts
/// only. A project's saved identity record and artifact-key memo stay on the
/// durable storage root that owns the cache root, and configure memoizes the
/// project's key under that durable root. Readers often name the cache root,
/// because that is where they open the artifacts; resolving the identity
/// there would find no record and could pick a different key, which names
/// folders configure never wrote. So a prepared cache root resolves to its
/// durable owner. Every other path is returned as given, which includes every
/// root while no `cache_dir` is configured: those keys do not change.
pub(crate) fn identity_storage_root(storage: &Path) -> std::borrow::Cow<'_, Path> {
    let registry = STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = registry
        .locations
        .get(storage)
        .and_then(|(_, _, identity)| identity.as_ref())
        .and_then(|identity| registry.owners.get(identity));
    match owner {
        Some(durable) if durable != storage => std::borrow::Cow::Owned(durable.clone()),
        _ => std::borrow::Cow::Borrowed(storage),
    }
}

/// The same location as `path`, spelled under the durable storage root that
/// owns the prepared `storage.cache_dir` cache root containing it, or `None`
/// when `path` is not inside such a cache root (always the case while no
/// `cache_dir` is configured).
///
/// Version-floor refusals belong to the storage root an artifact is reached
/// through. A cache root on another volume is never reached through its
/// storage root, so this gives its artifacts the route they would have if
/// the cache were linked under that storage root: `<cache>/aft/index/k` reads
/// as `<storage>/index/k`. That route is only a name for ownership; nothing
/// opens it.
pub(crate) fn durable_route(path: &Path) -> Option<PathBuf> {
    let registry = STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    registry
        .locations
        .iter()
        .filter_map(|(root, (_, _, identity))| {
            let durable = registry.owners.get(identity.as_ref()?)?;
            let relative = path.strip_prefix(root).ok()?;
            (durable != root).then(|| (root.components().count(), durable.join(relative)))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, route)| route)
}
fn status(root: &Path) -> (Option<String>, u64, Option<Identity>) {
    STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .locations
        .get(root)
        .cloned()
        .unwrap_or_default()
}
/// Give the configured spelling of an artifact root the same status as the
/// resolved root, so paths built from either spelling are admitted or refused
/// alike. Only the resolved root is recorded as the admitted identity.
fn mirror_status(configured: Option<&Path>, root: &Path) {
    let Some(configured) = configured.filter(|configured| *configured != root) else {
        return;
    };
    let mut roots = STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(entry) = roots.locations.get(root).cloned() {
        roots.locations.insert(configured.to_path_buf(), entry);
    }
}
fn set_status(root: &Path, reason: Option<String>, identity: Option<Identity>) -> u64 {
    let mut roots = STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if roots
        .locations
        .get(root)
        .is_none_or(|(old, _, old_identity)| {
            *old != reason || identity.is_some_and(|id| Some(id) != *old_identity)
        })
    {
        let identity = identity.or_else(|| roots.locations.get(root).and_then(|(_, _, id)| *id));
        let epoch = EPOCH.fetch_add(1, Ordering::Relaxed);
        roots
            .locations
            .insert(root.to_path_buf(), (reason.clone(), epoch, identity));
        if let Some(id) = identity {
            roots
                .admitted
                .insert(id, (root.to_path_buf(), reason, epoch));
        }
    }
    roots.locations[root].1
}

/// Creation below a configured artifact root must use non-recursive mkdirs:
/// an admitted worker must not recreate an unmounted/missing root later.
fn registered_cache_path(path: &Path) -> io::Result<Option<(PathBuf, Identity)>> {
    let (admitted, locations) = {
        let roots = STATUS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (roots.admitted.clone(), roots.locations.clone())
    };
    let mut unprepared = false;
    // Inspect enclosing identities before descendants, including case/bind
    // aliases, so a nested namespace cannot bypass its enclosing boundary.
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() => {
                let actual = identity(ancestor)?;
                if let Some((root, reason, _)) = admitted.get(&actual) {
                    // An inode can be reused after a retired temporary volume
                    // disappears. It grants nothing without its namespace or
                    // its still-current registered pathname.
                    if identity(root).ok() != Some(actual)
                        && !ancestor.join(OWNER).try_exists()?
                        && !path.starts_with(root)
                    {
                        continue;
                    }
                    let checked = (|| {
                        if let Some(reason) = reason {
                            return Err(io::Error::other(reason.clone()));
                        }
                        if real_directory(root)? != actual {
                            return Err(io::Error::other("artifact root identity changed"));
                        }
                        for ancestor in root.ancestors() {
                            if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
                                return Err(io::Error::other(
                                    "artifact root has a symlink ancestor",
                                ));
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = checked {
                        set_status(root, Some(error.to_string()), Some(actual));
                        return Err(io::Error::other(format!(
                            "{CACHE_DIR_UNAVAILABLE}: {} ({error})",
                            root.display()
                        )));
                    }
                    return Ok(Some((ancestor.to_path_buf(), actual)));
                }
                unprepared |= ancestor.join(OWNER).try_exists()?;
                unprepared |= locations.keys().any(|root| {
                    root.parent().and_then(|parent| identity(parent).ok()) == Some(actual)
                        && path
                            .strip_prefix(ancestor)
                            .ok()
                            .and_then(|rest| rest.components().next())
                            .is_some_and(|component| {
                                component
                                    .as_os_str()
                                    .to_string_lossy()
                                    .eq_ignore_ascii_case("aft")
                            })
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if let Some((_, (reason, _, _))) = locations.iter().find(|(root, _)| path.starts_with(root)) {
        return Err(io::Error::other(format!(
            "{CACHE_DIR_UNAVAILABLE}: {}",
            reason
                .as_deref()
                .unwrap_or("artifact root identity changed")
        )));
    }
    if unprepared {
        return Err(io::Error::other(format!(
            "{CACHE_DIR_UNAVAILABLE}: unprepared artifact namespace"
        )));
    }
    Ok(None)
}

/// A cache pathname is only a label. Keep the admitted directory open while
/// resolving descendants so replacement ancestors cannot redirect an operation.
pub(crate) struct CachePath {
    pub(crate) root: PinnedDir,
    pub(crate) relative: PathBuf,
}
impl CachePath {
    pub(crate) fn directory(&self, create: bool) -> io::Result<PinnedDir> {
        self.root.repair_descendant(&self.relative, create)
    }
    pub(crate) fn parent(&self) -> io::Result<(PinnedDir, std::ffi::OsString)> {
        let name = self
            .relative
            .file_name()
            .ok_or_else(|| io::Error::other("cache file requires a child name"))?
            .to_os_string();
        let parent = self
            .root
            .repair_descendant(self.relative.parent().unwrap_or(Path::new("")), false)?;
        Ok((parent, name))
    }
}

pub(crate) fn pin_cache_path(path: &Path) -> io::Result<Option<CachePath>> {
    // A nested alias must not replace the enclosing registered namespace as
    // the admission boundary. The pathname selects a candidate; its open
    // handle still has to match that registration before it grants access.
    let lexical = STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .locations
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(root, (reason, _, expected))| (root.clone(), reason.clone(), *expected));
    let selected = if let Some((root, reason, expected)) = lexical {
        if let Some(reason) = reason {
            return Err(io::Error::other(format!(
                "{CACHE_DIR_UNAVAILABLE}: {reason}"
            )));
        }
        Some((
            root,
            expected.ok_or_else(|| io::Error::other("unprepared artifact namespace"))?,
        ))
    } else {
        registered_cache_path(path)?
    };
    let Some((root_path, expected)) = selected else {
        return Ok(None);
    };
    let root = PinnedDir::repair_open_path(&root_path)?;
    if pinned_identity(&root)? != expected {
        return Err(io::Error::other(
            "cache_dir_unavailable: admitted artifact root identity changed while pinning",
        ));
    }
    let relative = path
        .strip_prefix(&root_path)
        .map_err(io::Error::other)?
        .to_path_buf();
    Ok(Some(CachePath { root, relative }))
}

pub(crate) fn check_cache_access(path: &Path) -> io::Result<Option<PathBuf>> {
    let Some(access) = pin_cache_path(path)? else {
        return Ok(None);
    };
    access.root.repair_validate_descendants(&access.relative)?;
    Ok(Some(access.root.path().to_path_buf()))
}

/// Prepare a folder that a third-party library will write into by pathname,
/// such as the hf-hub model downloader. Those libraries create any missing
/// folders themselves with a recursive mkdir, so if the cache volume vanished
/// after configure and left a writable mount point behind, handing them the
/// saved pathname would rebuild the cache path on the main disk. Call this
/// immediately before the hand-off.
///
/// Under a configured cache root this applies the same admission AFT's own
/// cache writes use: the root must still be the enrolled directory (same
/// device and inode) and the missing folders below it are created through the
/// pinned root, never recreating the root itself. Outside a cache root, a write
/// on durable storage whose caches were moved to `storage.cache_dir` is
/// refused. A refused root reports the `cache_dir_unavailable` reason.
pub(crate) fn prepare_third_party_cache_dir(path: &Path) -> Result<(), String> {
    let checked = (|| -> io::Result<()> {
        if let Some(access) = pin_cache_path(path)? {
            access.directory(true)?;
            return Ok(());
        }
        refuse_durable_cache_write(path)
    })();
    checked.map_err(|error| {
        let message = error.to_string();
        if message.starts_with(CACHE_DIR_UNAVAILABLE)
            || message.starts_with(CACHE_WRITE_ON_DURABLE_STORAGE)
        {
            message
        } else {
            format!("{CACHE_DIR_UNAVAILABLE}: {} ({message})", path.display())
        }
    })
}

pub const CACHE_WRITE_ON_DURABLE_STORAGE: &str = "cache_write_on_durable_storage";

/// Spellings under which a durable root is recognised: as configured, and
/// with its existing prefix resolved (for example macOS `/var` -> `/private/var`).
fn path_spellings(path: &Path) -> Vec<PathBuf> {
    let mut spellings = vec![path.to_path_buf()];
    let mut existing = path;
    let mut rest = Vec::new();
    while !existing.exists() {
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return spellings;
        };
        rest.push(name.to_os_string());
        existing = parent;
    }
    if let Ok(mut resolved) = fs::canonicalize(existing) {
        resolved.extend(rest.iter().rev());
        if resolved != path {
            spellings.push(resolved);
        }
    }
    spellings
}

fn record_split_durable(storage: &Path, split: bool) {
    let mut status = STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for spelling in path_spellings(storage) {
        if split {
            status.split_durable.insert(spelling);
        } else {
            status.split_durable.remove(&spelling);
        }
    }
}

/// Refuse to create a new rebuildable cache folder or database on a durable
/// root whose caches were moved to `storage.cache_dir`. Such a write would
/// silently put cache data back on the volume the setting is meant to relieve,
/// even while the configured cache volume is unavailable. Entries that already
/// exist there (caches from before the setting) stay readable and removable.
pub(crate) fn refuse_new_durable_cache_entry(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    refuse_durable_cache_write(path)
}

/// Existing legacy cache databases may be inspected or removed, but a writable
/// open could grow them and create WAL/SHM on the primary volume again.
pub(crate) fn refuse_durable_cache_write(path: &Path) -> io::Result<()> {
    let split = STATUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .split_durable
        .clone();
    if split.is_empty() {
        return Ok(());
    }
    for spelling in path_spellings(path) {
        for durable in &split {
            let Ok(relative) = spelling.strip_prefix(durable) else {
                continue;
            };
            let domain = relative
                .components()
                .next()
                .and_then(|component| match component {
                    std::path::Component::Normal(name) => name.to_str(),
                    _ => None,
                });
            if domain.is_some_and(|domain| CACHE_DOMAINS.contains(&domain)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{CACHE_WRITE_ON_DURABLE_STORAGE}: refusing to write {} on durable storage while its caches are split or their location is unknown",
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}
pub(crate) fn unavailable_warning(
    path: &Path,
    reason: &str,
) -> crate::config_resolve::ConfigWarning {
    let code = unavailable_code(reason);
    crate::config_resolve::ConfigWarning {
        code,
        key: "storage.cache_dir",
        tier: "user".into(),
        value: path.display().to_string(),
        message: if code == CACHE_DIR_BUSY {
            CACHE_DIR_BUSY_MESSAGE.to_string()
        } else {
            format!("storage.cache_dir {} is unavailable ({reason}); AFT is running without its caches until it is back.", path.display())
        },
    }
}

fn open_use_lock(root: &PinnedDir) -> io::Result<File> {
    root.repair_coordination_file(std::ffi::OsStr::new(IN_USE))
}
fn pinned_identity(root: &PinnedDir) -> io::Result<Identity> {
    let (device, inode) = root.directory_identity()?;
    Ok(Identity { device, inode })
}
/// A configured durable root may be a symlink. Resolve it at the operation's
/// boundary and retain the directory handle for every ownership comparison.
/// Artifact descendants still use no-follow walks, not this resolver.
fn pin_storage_root(storage: &Path) -> io::Result<PinnedDir> {
    let resolved = crate::bash_background::persistence::resolve_configured_root(storage)?;
    PinnedDir::repair_open_path(&resolved)
}
/// How long preparation waits for a conflicting exclusive hold on a root's
/// use lock (cache cleanup takes one briefly) before reporting the root busy.
#[cfg(not(test))]
const USE_LOCK_WAIT: Duration = Duration::from_secs(2);
#[cfg(test)]
const USE_LOCK_WAIT: Duration = Duration::from_millis(400);

fn take_shared_pinned(root: PinnedDir) -> io::Result<Arc<CacheLease>> {
    let file = open_use_lock(&root).map_err(|error| {
        // A bounded create/reopen failure already carries its operation, path
        // and retryable type; do not flatten it into a cache-folder failure.
        #[cfg(unix)]
        if error.get_ref().is_some_and(|error| {
            error.is::<crate::bash_background::persistence::CoordinationCreateContended>()
        }) {
            return error;
        }
        io::Error::new(
            error.kind(),
            format!(
                "open shared use lock {}: {error}",
                root.path().join(IN_USE).display()
            ),
        )
    })?;
    let deadline = std::time::Instant::now() + USE_LOCK_WAIT;
    loop {
        match file.try_lock_shared() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(storage_lock_error(error)),
        }
    }
    let root_identity = pinned_identity(&root)?;
    Ok(Arc::new(CacheLease {
        _file: file,
        root_identity,
        root,
    }))
}

fn has_ancestor(path: &Path, ancestor: Identity) -> io::Result<bool> {
    let mut current = fs::canonicalize(path)?;
    loop {
        if identity(&current)? == ancestor {
            return Ok(true);
        }
        if !current.pop() {
            return Ok(false);
        }
    }
}
fn overlap(left: &Path, right: &Path) -> io::Result<bool> {
    Ok(has_ancestor(left, identity(right)?)? || has_ancestor(right, identity(left)?)?)
}

pub fn prepare_cache_root(
    config: &mut crate::config::Config,
) -> Option<crate::config_resolve::ConfigWarning> {
    let storage = crate::storage_path::storage_dir(config.storage_dir.as_deref());
    if let Some(reason) = &config.cache_config_unknown {
        // The cache configuration is unknown. Treat durable storage as split
        // so database and directory creation there stays blocked until a valid
        // configuration explicitly selects the cache location.
        record_split_durable(&storage, true);
        config.cache_unavailable = Some(format!("cache_dir_config_unknown: {reason}"));
        config.cache_lease = None;
        return Some(crate::config_resolve::ConfigWarning {
            code: "cache_dir_config_unknown", key: "storage.cache_dir", tier: "user".into(), value: "unknown".into(),
            message: format!("User configuration is unreadable or invalid ({reason}); AFT is running without its caches until the configuration is valid."),
        });
    }
    // A prepared config holds the resolved directory in `cache_dir`. Recheck
    // the spelling the user configured instead, so a symlink in it that now
    // names another directory is refused rather than silently ignored. A
    // `cache_dir` changed since preparation is a new setting and is used as is.
    let requested = match (&config.cache_dir, &config.cache_dir_configured) {
        (Some(current), Some(configured)) if *current == configured.resolved => {
            Some(configured.spelling.clone())
        }
        (current, _) => current.clone(),
    };
    if requested.is_none() && !config.cache_dir_opt_out {
        // A storage root that once kept its caches on another volume must not
        // quietly start writing them to its own volume again because the
        // setting went missing (a typo, a deleted user file). Only an
        // explicit `"cache_dir": null` moves caches back.
        let message = match previous_cache_parent(&storage) {
            Ok(None) => None,
            Ok(Some(parent)) => Some(format!(
                "storage.cache_dir is not set, but this storage root's caches live on {}; set it again, or set \"cache_dir\": null to move caches back to the primary disk.",
                parent.display()
            )),
            Err(error) => Some(format!(
                "storage.cache_dir is not set, and this storage root's cache records cannot be read ({error}); set it again, or set \"cache_dir\": null to move caches back to the primary disk."
            )),
        };
        if let Some(message) = message {
            record_split_durable(&storage, true);
            config.cache_lease = None;
            config.cache_unavailable = Some(format!("{CACHE_DIR_UNAVAILABLE}: {message}"));
            return Some(crate::config_resolve::ConfigWarning {
                code: CACHE_DIR_UNAVAILABLE,
                key: "storage.cache_dir",
                tier: "user".into(),
                value: "unset".into(),
                message,
            });
        }
    }
    record_split_durable(&storage, requested.is_some());
    // The artifact root under the spelling the user configured. It differs
    // from the resolved root when that spelling runs through a symlink.
    let configured_root = requested.as_ref().map(|parent| parent.join("aft"));
    let mut root = cache_root(config);
    let result = (|| -> io::Result<Arc<CacheLease>> {
        if let Some(parent) = requested.as_ref() {
            if !parent.is_absolute() {
                return Err(io::Error::other("path must be absolute after ~ expansion"));
            }
            // Refuse an overlap with durable storage before anything about
            // this cache folder is registered or created.
            if paths_overlap(parent, &storage) {
                return Err(storage_overlap_error(parent, &storage));
            }
            // A configured path may run through symlinks the user chose (macOS
            // /var, a symlinked home, an alias of a /Volumes mount). Resolve it
            // once here; the resolved directory is then pinned by device and
            // inode, remembered with the configured spelling, and a later
            // change of what that spelling names is refused below.
            let configured = parent;
            let parent = crate::bash_background::persistence::resolve_configured_root(configured)?;
            let expected_parent = real_directory(&parent)?;
            for ancestor in parent.ancestors() {
                if fs::symlink_metadata(ancestor.join(OWNER)).is_ok() {
                    return Err(io::Error::other(
                        "cache root overlaps another configured artifact root",
                    ));
                }
            }
            // Check before creating even the durable directory: an unavailable
            // volume must not cause an artifact write at any alternate location.
            root = parent.join("aft");
            config.cache_dir_configured = Some(crate::config::ConfiguredCacheDir {
                spelling: configured.clone(),
                resolved: parent.clone(),
            });
            config.cache_dir = Some(parent.clone());
            let storage_exists = storage.exists();
            let mut existing = storage.as_path();
            while !existing.exists() {
                existing = existing
                    .parent()
                    .ok_or_else(|| io::Error::other("storage ancestor unavailable"))?;
            }
            let durable_pin = if storage_exists {
                pin_storage_root(existing)?
            } else {
                PinnedDir::pin_unregistered_root(existing)?
            };
            // The same check by directory identity, which also catches an
            // overlap through a bind mount or another alias.
            if storage_exists {
                if overlap(&parent, durable_pin.path())? {
                    return Err(storage_overlap_error(configured, &storage));
                }
            } else {
                if has_ancestor(durable_pin.path(), expected_parent)? {
                    return Err(storage_overlap_error(configured, &storage));
                }
            }
            let roots = STATUS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .locations
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            for other in roots {
                if other == root
                    || Some(&other) == configured_root.as_ref()
                    || !other.exists()
                    // Another spelling of this same root is not an overlap.
                    || crate::bash_background::persistence::canonical_root_path(&other)
                        .ok()
                        .as_ref()
                        == Some(&root)
                {
                    continue;
                }
                #[cfg(test)]
                BEFORE_OTHER_ROOT_OVERLAP.with(|hook| {
                    let mut hook = hook.borrow_mut();
                    if hook.as_ref().is_some_and(|(path, _)| path == &other) {
                        if let Some((_, hook)) = hook.take() {
                            hook();
                        }
                    }
                });
                let overlaps = match overlap(&parent, &other) {
                    Ok(overlaps) => overlaps,
                    // Registered labels outlive their roots. Another operation
                    // may retire this root after the existence check above;
                    // that must not disable an unrelated cache's enrollment.
                    Err(_) if !other.try_exists()? => continue,
                    Err(error) => return Err(error),
                };
                if overlaps {
                    return Err(io::Error::other(
                        "cache root overlaps another configured artifact root",
                    ));
                }
            }
            let parent_pin = PinnedDir::open(&parent)?;
            if pinned_identity(&parent_pin)? != expected_parent {
                return Err(io::Error::other(
                    "cache parent identity changed during prepare",
                ));
            }
            validate_saved_parent(&storage, configured, &parent, expected_parent)?;
            check_parent_mount(&parent, expected_parent)?;
            let first_enrollment = !read_parent_state(&storage)?
                .parents
                .iter()
                .any(|saved| saved.path == parent);
            let primary_device = if first_enrollment {
                #[cfg(test)]
                BEFORE_ENROLLMENT_DEVICE.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook();
                    }
                });
                let primary_device = durable_pin.cache_device()?;
                if parent_pin.cache_device()? == primary_device {
                    return Err(io::Error::other("cache_dir_on_storage_device: cache directory must be on a different volume from durable storage"));
                }
                Some(primary_device)
            } else {
                None
            };
            let probe = format!(
                ".aft-probe-{}-{}",
                std::process::id(),
                EPOCH.fetch_add(1, Ordering::Relaxed)
            );
            parent_pin.repair_new_file(std::ffi::OsStr::new(&probe))?;
            parent_pin.repair_remove_file(std::ffi::OsStr::new(&probe))?;
            crate::private_storage::open_root(&storage)?;
            let storage_pin = if storage_exists {
                durable_pin
            } else {
                pin_storage_root(&storage)?
            };
            crate::reader_floor::prepare(&storage);
            reconcile_root_history(&storage_pin)?;
            let (root_pin, enrollment) = match parent_pin.repair_child(std::ffi::OsStr::new("aft"))
            {
                Ok(root) => (root, None),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let mut random = [0u8; 32];
                    getrandom::fill(&mut random).map_err(io::Error::other)?;
                    let registration = random
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    // Save an unfinished enrollment before publishing `aft`.
                    // Retries create a fresh staging folder rather than adopting
                    // an orphan; pending records protect any surviving folders.
                    let name = std::ffi::OsString::from(format!(".aft-enrolling-{registration}"));
                    let root = parent_pin.repair_create_dir(&name)?;
                    (root, Some((name, registration)))
                }
                Err(error) => return Err(error),
            };
            let created = enrollment.is_some();
            #[cfg(test)]
            if created {
                AFTER_CACHE_ROOT_CREATE.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook();
                    }
                });
            }
            let expected_root = pinned_identity(&root_pin)?;
            if let Some(primary_device) = primary_device {
                if root_pin.cache_device()? == primary_device {
                    return Err(io::Error::other("cache_dir_on_storage_device: artifact namespace is on the durable storage volume"));
                }
                if root_pin.cache_device()? != parent_pin.cache_device()? {
                    return Err(io::Error::other(
                        "artifact namespace is on a different volume than its enrolled parent",
                    ));
                }
            }
            let owner = pinned_identity(&storage_pin)?;
            let registration = if let Some((_, registration)) = &enrollment {
                registration.clone()
            } else {
                marker_matches_pinned(&root_pin, &storage_pin)?
            };
            let mut lease = take_shared_pinned(root_pin)?;
            if real_directory(lease.root.path())? != expected_root
                || lease.root_identity != expected_root
            {
                return Err(io::Error::other(
                    "artifact root identity changed during prepare",
                ));
            }
            match lease.root.repair_read_file(std::ffi::OsStr::new(OWNER)) {
                Ok(file) => {
                    let marker = read_marker_identity(file)?;
                    if marker.owner != owner {
                        return Err(foreign_owner_error());
                    }
                    if marker.registration != registration {
                        return Err(io::Error::other(
                            "artifact registration changed during prepare",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !created {
                        return Err(io::Error::other(
                            "artifact root has no AFT ownership marker",
                        ));
                    }
                    let mut marker = lease.root.repair_new_file(std::ffi::OsStr::new(OWNER))?;
                    use std::io::Write;
                    marker.write_all(
                        &serde_json::to_vec(&OwnerMarker {
                            owner,
                            registration: registration.clone(),
                        })
                        .map_err(io::Error::other)?,
                    )?;
                    marker.sync_all()?;
                }
                Err(error) => return Err(error),
            }
            lease.root.repair_private_dir()?;
            lease.root.repair_sync()?;
            remember_roots(
                &storage_pin,
                &root,
                expected_root,
                &registration,
                enrollment.as_ref().map(|_| lease.root.path()),
            )?;
            remember_parent(&storage, configured, &parent, expected_parent)?;
            // Publication and cleanup share the history lock, so a pending
            // folder cannot move out from under cleanup's protection snapshot.
            let _publication_lock = lock_root_history(&storage)?;
            if let Some((name, _)) = enrollment {
                let source = parent_pin.repair_entry_for_rename(&name)?;
                if pinned_identity(&source.directory()?)? != expected_root {
                    return Err(io::Error::other("enrollment source identity changed"));
                }
                // Windows refuses to rename a directory while a file inside it
                // is open (ERROR_ACCESS_DENIED), and the staging lease holds
                // `.aft-in-use.lock` open in the folder being published. Close
                // that lease first. Cleanup cannot step in meanwhile: it needs
                // the history lock held above, and the pending record still
                // protects the staging folder. The lease is taken again on
                // `aft` right after the rename.
                #[cfg(windows)]
                drop(lease);
                // No-replace publication must refuse even an empty foreign folder
                // or symlink installed at the destination during registration.
                parent_pin.repair_rename_entry_to(
                    &name,
                    &source,
                    &parent_pin,
                    std::ffi::OsStr::new("aft"),
                )?;
                #[cfg(test)]
                AFTER_CACHE_ROOT_RENAME.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook();
                    }
                });
                parent_pin.repair_sync()?;
                // Reopen `aft` after the rename so the lease uses its final path
                // while retaining the identity of the same directory.
                lease = take_shared_pinned(parent_pin.repair_child(std::ffi::OsStr::new("aft"))?)?;
            }
            if real_directory(&root)? != expected_root || lease.root_identity != expected_root {
                return Err(io::Error::other(
                    "artifact root identity changed during publication",
                ));
            }
            reconcile_root_history_locked(&storage_pin)?;
            Ok(lease)
        } else {
            crate::private_storage::open_root(&storage)?;
            let storage_pin = pin_storage_root(&storage)?;
            crate::reader_floor::prepare(&storage);
            reconcile_root_history(&storage_pin)?;
            take_shared_pinned(storage_pin)
        }
    })();
    match result {
        Ok(lease) => {
            STATUS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .owners
                .insert(lease.root_identity, storage.clone());
            config.cache_lease = Some(Arc::clone(&lease));
            config.cache_unavailable = None;
            config.cache_epoch = if requested.is_some() {
                set_status(&root, None, Some(lease.root_identity))
            } else {
                0
            };
            mirror_status(configured_root.as_deref(), &root);
            config.semantic.local_cache_dir =
                requested.as_ref().map(|_| root.join("semantic/models"));
            crate::project_content::register_storage_root(&root);
            crate::reader_floor::prepare_artifact_root(&storage, &root);
            None
        }
        Err(error) => {
            config.cache_lease = None;
            config.cache_unavailable = Some(error.to_string());
            // Reject the second store without quarantining the legitimate
            // owner's healthy artifact root in this process.
            let foreign_owner = error
                .get_ref()
                .is_some_and(|error| error.is::<ForeignCacheOwner>());
            // A busy use lock (a cleanup holding it past the bounded wait) is
            // transient: this config reports it, and the next prepare retries.
            // It is never recorded in the process-wide registry, which would
            // refuse every later path under the root until a restart.
            let busy = error.kind() == io::ErrorKind::WouldBlock;
            let storage_overlap = error
                .get_ref()
                .is_some_and(|error| error.is::<StorageOverlap>())
                || paths_overlap(&root, &storage);
            if storage_overlap && requested.is_some() {
                // Drop the "not prepared" placeholders `cache_root` may have
                // added, and keep it from adding them again.
                let mut registry = STATUS
                    .get_or_init(Default::default)
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                for refused in std::iter::once(&root).chain(configured_root.as_ref()) {
                    registry.locations.remove(refused);
                    registry.storage_overlaps.insert(refused.clone());
                }
            }
            config.cache_epoch = if requested.is_none() || storage_overlap {
                // Without `storage.cache_dir` the "cache root" is the durable
                // storage root itself, and an overlapping cache root contains
                // durable storage or sits inside it. The registry only
                // describes separate cache roots; recording either there
                // would block backups, bash task records and every other
                // durable write.
                0
            } else if foreign_owner || busy {
                status(&root).1
            } else {
                set_status(&root, Some(error.to_string()), None)
            };
            mirror_status(configured_root.as_deref(), &root);
            config.semantic.local_cache_dir = None;
            Some(unavailable_warning(
                requested.as_deref().unwrap_or(&root),
                &error.to_string(),
            ))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RecordedRoot {
    path: PathBuf,
    identity: Option<Identity>,
    #[serde(default)]
    registration: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct OwnerMarker {
    owner: Identity,
    registration: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct History {
    version: u32,
    roots: Vec<RecordedRoot>,
    #[serde(default)]
    pending: Vec<PendingRoot>,
}
#[derive(Debug, Serialize, Deserialize)]
struct PendingRoot {
    root: RecordedRoot,
    staging: PathBuf,
}
fn history_path(storage: &Path) -> PathBuf {
    storage.join("retention/cache-roots.json")
}
/// The cache parent this storage root last used, from its durable records
/// (enrolled cache parents, then recorded artifact roots); None when it has
/// always kept its caches on itself.
fn previous_cache_parent(storage: &Path) -> io::Result<Option<PathBuf>> {
    if let Some(saved) = read_parent_state(storage)?.parents.last() {
        return Ok(Some(
            saved
                .configured
                .clone()
                .unwrap_or_else(|| saved.path.clone()),
        ));
    }
    let bytes = match fs::read(history_path(storage)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let roots = match serde_json::from_slice::<History>(&bytes) {
        Ok(history) => history
            .roots
            .into_iter()
            .chain(history.pending.into_iter().map(|pending| pending.root))
            .map(|root| root.path)
            .collect(),
        Err(_) => serde_json::from_slice::<Vec<PathBuf>>(&bytes).map_err(io::Error::other)?,
    };
    Ok(roots
        .into_iter()
        .rev()
        .find(|root| root != storage)
        .map(|root| root.parent().map(Path::to_path_buf).unwrap_or(root)))
}
fn remember_roots(
    storage: &PinnedDir,
    artifacts: &Path,
    artifact_identity: Identity,
    registration: &str,
    staging: Option<&Path>,
) -> io::Result<()> {
    let _lock = lock_root_history(storage.path())?;
    let mut history = read_root_history(storage.path())?;
    reconcile_pending_roots(storage, &mut history)?;
    let record = RecordedRoot {
        path: artifacts.to_path_buf(),
        identity: Some(artifact_identity),
        registration: Some(registration.to_owned()),
    };
    if let Some(staging) = staging {
        history.pending.push(PendingRoot {
            root: record,
            staging: staging.to_path_buf(),
        });
    } else {
        add_published_root(&mut history, record);
    }
    #[cfg(test)]
    BEFORE_ROOT_HISTORY_WRITE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook()?;
        }
        Ok::<_, io::Error>(())
    })?;
    write_root_history(storage.path(), &history)
}

fn lock_root_history(storage: &Path) -> io::Result<crate::fs_lock::LockGuard> {
    crate::private_storage::open_dir(storage, &storage.join("retention"))?;
    crate::fs_lock::try_acquire(
        &storage.join("retention/cache-roots.lock"),
        Duration::from_secs(5),
    )
    .map_err(io::Error::other)
}

fn read_root_history(storage: &Path) -> io::Result<History> {
    Ok(match fs::read(history_path(storage)) {
        Ok(bytes) => match serde_json::from_slice::<History>(&bytes) {
            Ok(history) if history.version == 2 => history,
            _ => History {
                version: 2,
                roots: serde_json::from_slice::<Vec<PathBuf>>(&bytes)
                    .map_err(io::Error::other)?
                    .into_iter()
                    .map(|path| RecordedRoot {
                        path,
                        identity: None,
                        registration: None,
                    })
                    .collect(),
                pending: Vec::new(),
            },
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => History {
            version: 2,
            roots: Vec::new(),
            pending: Vec::new(),
        },
        Err(error) => return Err(error),
    })
}

fn add_published_root(history: &mut History, record: RecordedRoot) {
    if !history.roots.iter().any(|old| {
        old.path == record.path
            && old.identity == record.identity
            && old.registration == record.registration
    }) {
        history.roots.push(record);
    }
}

fn reconcile_pending_roots(storage: &PinnedDir, history: &mut History) -> io::Result<bool> {
    let owner = pinned_identity(storage)?;
    let mut changed = false;
    for pending in std::mem::take(&mut history.pending) {
        if recorded_root_matches(&pending.root).is_ok() {
            let root = PinnedDir::repair_open_path(&pending.root.path)?;
            // Enrollment writes and syncs the ownership marker in the staging
            // folder before it records the pending root, so a published root
            // always carries one. A folder at the published path with a
            // matching identity but no marker is a foreign folder that reused
            // the deleted staging folder's inode (ext4 and tmpfs do this
            // readily). It is not adopted; the staging check below decides
            // the record, as for any other unpublished root.
            let marker = match root.repair_read_file(std::ffi::OsStr::new(OWNER)) {
                Ok(file) => Some(read_marker_identity(file)?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            if let Some(marker) = marker {
                if Some(pinned_identity(&root)?) != pending.root.identity
                    || marker.owner != owner
                    || Some(marker.registration.as_str()) != pending.root.registration.as_deref()
                {
                    return Err(io::Error::other("pending artifact registration changed"));
                }
                add_published_root(history, pending.root);
                changed = true;
                continue;
            }
        }
        match fs::symlink_metadata(&pending.staging) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && Some(real_directory(&pending.staging)?) == pending.root.identity =>
            {
                // A surviving staging folder is not published history, but
                // cleanup must still protect it, including through aliases.
                history.pending.push(pending);
            }
            Ok(_) => changed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => changed = true,
            Err(error) => return Err(error),
        }
    }
    Ok(changed)
}

fn reconcile_root_history(storage: &PinnedDir) -> io::Result<()> {
    if !history_path(storage.path()).try_exists()? {
        return Ok(());
    }
    let _lock = lock_root_history(storage.path())?;
    reconcile_root_history_locked(storage)
}

fn reconcile_root_history_locked(storage: &PinnedDir) -> io::Result<()> {
    let mut history = read_root_history(storage.path())?;
    if reconcile_pending_roots(storage, &mut history)? {
        write_root_history(storage.path(), &history)?;
    }
    Ok(())
}

fn write_root_history(storage: &Path, history: &History) -> io::Result<()> {
    crate::durability::atomic_write(
        &history_path(storage),
        &serde_json::to_vec(history).map_err(io::Error::other)?,
        crate::durability::AtomicWriteDurability::FileAndDirectory,
    )
}

#[derive(Debug, Default, Serialize)]
pub struct CleanupReport {
    pub removed: Vec<PathBuf>,
    pub skipped: Vec<String>,
    pub partial: Vec<PartialRemoval>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct CleanupCandidate {
    pub path: PathBuf,
    pub bytes: u64,
}
#[derive(Debug, Serialize)]
pub struct PartialRemoval {
    pub path: PathBuf,
    pub removed_entries: usize,
}
fn recorded_root_matches(record: &RecordedRoot) -> io::Result<()> {
    if Some(real_directory(&record.path)?) != record.identity {
        return Err(io::Error::other(
            "recorded root identity changed or unverified",
        ));
    }
    Ok(())
}
fn read_marker_identity(file: File) -> io::Result<OwnerMarker> {
    if file.metadata()?.len() > 512 {
        return Err(io::Error::other("ownership marker is too large"));
    }
    serde_json::from_reader(file).map_err(io::Error::other)
}
fn marker_matches_pinned(root: &PinnedDir, storage: &PinnedDir) -> io::Result<String> {
    let recorded = read_marker_identity(root.repair_read_file(std::ffi::OsStr::new(OWNER))?)?;
    if recorded.owner != pinned_identity(storage)? {
        return Err(foreign_owner_error());
    }
    let history: History = serde_json::from_slice(&fs::read(history_path(storage.path()))?)
        .map_err(io::Error::other)?;
    let actual = pinned_identity(root)?;
    if !history.roots.iter().any(|entry| {
        entry.identity == Some(actual)
            && entry.registration.as_deref() == Some(recorded.registration.as_str())
    }) {
        return Err(io::Error::other(
            "cannot enroll an unregistered artifact namespace",
        ));
    }
    Ok(recorded.registration)
}
fn known_harness(name: &str) -> bool {
    crate::engram_catalog::harness_label(name).is_some()
}
fn candidates_and_durable(storage: &Path) -> io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    candidates_and_durable_with_budget(storage, &mut RepairBudget::new())
}

fn candidates_and_durable_with_budget(
    storage: &Path,
    budget: &mut RepairBudget,
) -> io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut candidates = CACHE_DOMAINS
        .iter()
        .map(|name| storage.join(name))
        .collect::<Vec<_>>();
    let mut durable = Vec::new();
    let mut entries = fs::read_dir(storage)?;
    loop {
        budget.entry()?;
        let Some(entry) = entries.next() else { break };
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| CACHE_DOMAINS.contains(&name))
        {
            continue;
        }
        if name.to_str().is_some_and(known_harness) && entry.file_type()?.is_dir() {
            let legacy = [entry.path().join("callgraph"), entry.path().join("inspect")];
            candidates.extend(legacy);
            let mut children = fs::read_dir(entry.path())?;
            loop {
                budget.entry()?;
                let Some(child) = children.next() else { break };
                let child = child?;
                if !matches!(child.file_name().to_str(), Some("callgraph" | "inspect")) {
                    durable.push(child.path());
                }
            }
        } else {
            durable.push(entry.path());
        }
    }
    Ok((candidates, durable))
}

pub(crate) fn deletion_protections(paths: &[PathBuf]) -> io::Result<Vec<(u64, u64)>> {
    deletion_protections_with_budget(paths, &mut RepairBudget::new(), &mut Default::default())
}

fn deletion_protections_with_budget(
    paths: &[PathBuf],
    budget: &mut RepairBudget,
    examined: &mut crate::bash_background::persistence::IdentityCensus,
) -> io::Result<Vec<(u64, u64)>> {
    let mut protected = Vec::new();
    for path in paths {
        budget.entry()?;
        if !path.try_exists()? {
            continue;
        }
        // A path removed after the check above has nothing left to protect,
        // the same as one that never existed.
        let canonical = match fs::canonicalize(path) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if canonical.is_dir() {
            match PinnedDir::open(&canonical) {
                Ok(pinned) => pinned.directory_identities(&mut protected, budget, examined)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        for ancestor in canonical.ancestors() {
            budget.entry()?;
            let id = identity(ancestor)?;
            protected.push((id.device, id.inode));
        }
    }
    Ok(protected)
}
fn candidate_protected(
    candidate: &Path,
    storage: &Path,
    active: &[PathBuf],
    durable: &[PathBuf],
) -> io::Result<bool> {
    let candidate_id = real_directory(candidate)?;
    if has_ancestor(storage, candidate_id)? {
        return Ok(true);
    }
    for protected in active.iter().chain(durable) {
        match fs::symlink_metadata(protected) {
            Ok(_) => {
                if overlap(candidate, protected)? {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}
fn ensure_cache_idle(
    root: &Path,
    protected: &[(u64, u64)],
) -> io::Result<Vec<crate::fs_lock::LockGuard>> {
    fn visit(
        dir: &PinnedDir,
        root: &PinnedDir,
        protected: &[(u64, u64)],
        budget: &mut RepairBudget,
        depth: usize,
    ) -> io::Result<()> {
        budget.enter(depth)?;
        dir.repair_check_boundary(root, protected)?;
        if markers::sweep_all_read_markers_pinned(dir, budget, false)?.protected {
            return Err(io::Error::other("cache has live readers"));
        }
        for name in dir.repair_names(budget)? {
            match dir.repair_child(&name) {
                Ok(child) => {
                    if name == "pins" && !child.repair_names(budget)?.is_empty() {
                        return Err(io::Error::other(
                            "cache has pins; stop readers before cleanup",
                        ));
                    }
                    visit(&child, root, protected, budget, depth + 1)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                    dir.repair_read_file(&name).map_err(|error| {
                        io::Error::other(format!(
                            "cache preflight refuses symlink or non-regular file: {error}"
                        ))
                    })?;
                    if matches!(
                        name.to_str(),
                        Some("writer.lease" | "cache.lock" | "symbols.lock" | "semantic.lock")
                    ) {
                        // Cleanup holds the namespace's exclusive use lock. A
                        // leftover coordination file is not ours to reclaim or
                        // heartbeat, even if its contents look stale.
                        return Err(io::Error::other("cache coordination lock exists; stop its owner and repair the lock before cleanup"));
                    }
                }
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "cache preflight refuses symlink or non-directory: {error}"
                    )))
                }
            }
        }
        Ok(())
    }
    let root = PinnedDir::open(root)?;
    visit(&root, &root, protected, &mut RepairBudget::new(), 0)?;
    Ok(Vec::new())
}

fn cleanup_roots(
    storage: &Path,
    active: &[PathBuf],
    report: &mut CleanupReport,
    legacy_root: Option<&PinnedDir>,
    mut confirm: Option<&mut dyn FnMut(&[CleanupCandidate]) -> bool>,
) -> io::Result<()> {
    let legacy = legacy_root.is_some();
    let storage_pin = match legacy_root {
        Some(root) => root.repair_clone()?,
        None => pin_storage_root(storage)?,
    };
    let _history_lock = lock_root_history(storage)?;
    let bytes = match fs::read(history_path(storage)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            b"{\"version\":2,\"roots\":[]}".to_vec()
        }
        Err(error) => return Err(error),
    };
    let mut history = match serde_json::from_slice::<History>(&bytes) {
        Ok(history) if history.version == 2 => history,
        _ => {
            if let Ok(legacy) = serde_json::from_slice::<Vec<PathBuf>>(&bytes) {
                for path in legacy {
                    report.skipped.push(format!(
                        "{}: unverified legacy root has no recorded identity/ownership",
                        path.display()
                    ));
                }
            } else {
                report
                    .skipped
                    .push("unrecognized cache root history; no roots removed".into());
            }
            return Ok(());
        }
    };
    if reconcile_pending_roots(&storage_pin, &mut history)? {
        write_root_history(storage, &history)?;
    }
    let pending_paths = history
        .pending
        .into_iter()
        .map(|pending| pending.staging)
        .collect::<Vec<_>>();
    let mut records = history.roots;
    let owner = pinned_identity(&storage_pin)?;
    if legacy {
        records = vec![RecordedRoot {
            path: storage_pin.path().to_path_buf(),
            identity: Some(owner),
            registration: None,
        }];
    }
    let (default_candidates, mut durable) = if let Some(root) = legacy_root {
        let mut durable = Vec::new();
        for name in root.repair_names(&mut RepairBudget::new())? {
            if !name
                .to_str()
                .is_some_and(|name| CACHE_DOMAINS.contains(&name))
            {
                durable.push(storage.join(name));
            }
        }
        (
            CACHE_DOMAINS
                .iter()
                .map(|domain| storage.join(domain))
                .collect(),
            durable,
        )
    } else {
        candidates_and_durable(storage)?
    };
    durable.extend(pending_paths.iter().cloned());
    let mut protected_paths = if legacy {
        durable.clone()
    } else {
        vec![storage.to_path_buf()]
    };
    protected_paths.extend_from_slice(active);
    protected_paths.extend(pending_paths);
    let protected = deletion_protections(&protected_paths)?;
    let mut plans = Vec::new();
    let mut refusal = false;
    for record in records {
        let is_storage = record.identity == Some(owner);
        if is_storage && !legacy {
            continue;
        }
        if let Err(error) = recorded_root_matches(&record) {
            report.skipped.push(format!(
                "{}: root identity/symlink refusal: {error}",
                record.path.display()
            ));
            refusal = true;
            continue;
        }
        let pinned = match PinnedDir::repair_open_path(&record.path).and_then(|pinned| {
            if Some(pinned_identity(&pinned)?) != record.identity {
                return Err(io::Error::other("recorded root changed while opening"));
            }
            Ok(pinned)
        }) {
            Ok(pinned) => pinned,
            Err(error) => {
                report.skipped.push(format!(
                    "{}: root identity/symlink refusal: {error}",
                    record.path.display()
                ));
                refusal = true;
                continue;
            }
        };
        if active
            .iter()
            .any(|path| identity(path).ok() == record.identity)
        {
            continue;
        }
        if !is_storage {
            if let Err(error) = marker_matches_pinned(&pinned, &storage_pin) {
                report
                    .skipped
                    .push(format!("{}: unowned root: {error}", record.path.display()));
                refusal = true;
                continue;
            }
        }
        let exclusive = open_use_lock(&pinned)?;
        exclusive.try_lock().map_err(|error| {
            io::Error::other(format!(
                "artifact root {} is in use: {error}",
                record.path.display()
            ))
        })?;
        recorded_root_matches(&record)?;
        let candidates = if is_storage {
            default_candidates.clone()
        } else {
            CACHE_DOMAINS
                .iter()
                .map(|domain| record.path.join(domain))
                .collect()
        };
        let mut admitted = Vec::new();
        for candidate in candidates {
            recorded_root_matches(&record)?;
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    if legacy {
                        return Err(io::Error::other(
                            "old cache candidate is not a real directory",
                        ));
                    }
                    report.skipped.push(format!(
                        "{}: candidate is not a real directory",
                        candidate.display()
                    ));
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            if candidate_protected(&candidate, storage_pin.path(), active, &durable)? {
                if legacy {
                    return Err(io::Error::other(
                        "old cache candidate aliases active or durable storage",
                    ));
                }
                report.skipped.push(format!(
                    "{}: active artifact or durable subtree",
                    candidate.display()
                ));
                continue;
            }
            let candidate_identity = real_directory(&candidate)?;
            if candidate_identity.device != pinned_identity(&pinned)?.device {
                return Err(io::Error::other(
                    "cache candidate is a mount on another device",
                ));
            }
            crate::production_storage::refuse_write(&candidate)?;
            let base = if is_storage {
                storage
            } else {
                record.path.as_path()
            };
            let relative = candidate.strip_prefix(base).map_err(io::Error::other)?;
            let mut components = relative.components().collect::<Vec<_>>();
            let leaf = components
                .pop()
                .ok_or_else(|| io::Error::other("missing cache child name"))?;
            let mut parent = pinned.repair_clone()?;
            for component in components {
                parent = parent.repair_child_same_mount(component.as_os_str())?;
            }
            let leaf = leaf.as_os_str().to_os_string();
            // Verify every mount and protected-directory boundary before the
            // read-only liveness census examines any candidate entries.
            parent.repair_child_tree_bytes(&leaf, &protected)?;
            let locks = ensure_cache_idle(&candidate, &protected)?;
            recorded_root_matches(&record)?;
            if real_directory(&candidate)? != candidate_identity {
                return Err(io::Error::other(
                    "cache candidate identity changed during repair",
                ));
            }
            admitted.push((candidate, parent, leaf, candidate_identity, locks));
        }
        plans.push((record, exclusive, admitted));
    }
    if refusal {
        return Err(io::Error::other(
            "cache cleanup preflight refused; no roots removed",
        ));
    }
    let mut inventory = Vec::new();
    for (_, _, admitted) in &plans {
        for (candidate, parent, leaf, expected, _) in admitted {
            let child = parent.repair_child(leaf)?;
            if pinned_identity(&child)? != *expected {
                return Err(io::Error::other(
                    "cache candidate identity changed during preflight",
                ));
            }
            inventory.push(CleanupCandidate {
                path: candidate.clone(),
                bytes: parent.repair_child_tree_bytes(leaf, &protected)?,
            });
        }
    }
    if let Some(confirm) = confirm.as_mut() {
        if !confirm(&inventory) {
            report.reason = Some("old_cache_confirmation_required");
            return Err(io::Error::other(
                "old cache cleanup requires exact confirmation",
            ));
        }
    }
    for (record, _exclusive, admitted) in plans {
        for (candidate, parent, leaf, candidate_identity, _locks) in admitted {
            recorded_root_matches(&record)?;
            let mut removed = 0;
            #[cfg(test)]
            BEFORE_REPAIR_REMOVE.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            if let Err(error) = parent.repair_remove_child_with_warnings(
                &leaf,
                (candidate_identity.device, candidate_identity.inode),
                &protected,
                &mut removed,
                &mut report.warnings,
            ) {
                if removed > 0 {
                    report.partial.push(PartialRemoval {
                        path: candidate.clone(),
                        removed_entries: removed,
                    });
                }
                return Err(error);
            }
            report.removed.push(candidate);
            recorded_root_matches(&record)?;
        }
    }
    Ok(())
}

pub(crate) fn remove_inactive_cache_roots(storage: &Path, active: &[PathBuf]) -> CleanupReport {
    let mut report = CleanupReport::default();
    if let Err(error) = cleanup_roots(storage, active, &mut report, None, None) {
        report.error = Some(error.to_string());
    }
    report
}

#[cfg(test)]
fn remove_legacy_cache_roots(storage: &Path, active: &[PathBuf]) -> CleanupReport {
    let mut report = CleanupReport::default();
    let root = match pin_storage_root(storage) {
        Ok(root) => root,
        Err(error) => {
            report.error = Some(error.to_string());
            return report;
        }
    };
    if let Err(error) = cleanup_roots(storage, active, &mut report, Some(&root), None) {
        report.error = Some(error.to_string());
    }
    report
}

/// Legacy cache deletion is a separate operation, never automatic repair.
/// The callback sees the fully preflighted inventory while all locks are held.
pub fn clean_old_caches(
    tiers: &[crate::config_resolve::ConfigTier],
    storage: &Path,
    confirm: impl FnMut(&[CleanupCandidate]) -> bool,
) -> CleanupReport {
    clean_old_caches_resolved(
        tiers,
        storage,
        &crate::storage_path::storage_dir(None),
        confirm,
    )
}

fn clean_old_caches_resolved(
    tiers: &[crate::config_resolve::ConfigTier],
    storage: &Path,
    resolved: &Path,
    mut confirm: impl FnMut(&[CleanupCandidate]) -> bool,
) -> CleanupReport {
    let mut report = CleanupReport::default();
    let work = (|| -> io::Result<()> {
        let result = crate::config_resolve::resolve_config(tiers);
        if !result.errors.is_empty() {
            return Err(io::Error::other(format!(
                "invalid user config: {:?}",
                result.errors
            )));
        }
        let mut config = result.config;
        if let Some(reason) = &config.cache_config_unknown {
            report.reason = Some("cache_dir_config_unknown");
            return Err(io::Error::other(reason.clone()));
        }
        if config.cache_dir.is_none() {
            report.reason = Some("old_cache_external_root_required");
            return Err(io::Error::other(
                "old caches are active: configure an available external storage.cache_dir first",
            ));
        }
        let storage_pin = pin_storage_root(storage)?;
        if pin_storage_root(resolved)
            .and_then(|root| pinned_identity(&root))
            .ok()
            != Some(pinned_identity(&storage_pin)?)
        {
            report.reason = Some("old_cache_storage_root_mismatch");
            return Err(io::Error::other(
                "old-cache cleanup requires the resolved durable storage root",
            ));
        }
        if !storage_root_proven(&storage_pin)? {
            report.reason = Some("old_cache_storage_root_unproven");
            return Err(io::Error::other(
                "old-cache cleanup requires aft.db or the storage root's AFT owner marker",
            ));
        }
        let _cleanup_lease = acquire_cache_cleanup_lease(storage)?;
        config.storage_dir = Some(storage.to_path_buf());
        if let Some(warning) = prepare_cache_root(&mut config) {
            report.reason = Some(warning.code);
            return Err(io::Error::other(warning.message));
        }
        let active = config
            .cache_lease
            .as_ref()
            .ok_or_else(|| io::Error::other("cache root is not enrolled"))?;
        if active.root.cache_device()? == storage_pin.cache_device()? {
            report.reason = Some("old_cache_external_root_required");
            return Err(io::Error::other(
                "old-cache cleanup requires an active enrolled cache root on its own device",
            ));
        }
        cleanup_roots(
            storage,
            &[cache_root(&config)],
            &mut report,
            Some(&storage_pin),
            Some(&mut confirm),
        )
    })();
    if let Err(error) = work {
        report.error = Some(error.to_string());
    }
    report
}

#[cfg(test)]
pub(crate) fn clean_old_caches_for_test(
    tiers: &[crate::config_resolve::ConfigTier],
    storage: &Path,
    confirm: impl FnMut(&[CleanupCandidate]) -> bool,
) -> CleanupReport {
    let parent = crate::config_resolve::resolve_config(tiers)
        .config
        .cache_dir;
    let _volume = parent
        .as_ref()
        .filter(|parent| parent.is_dir())
        .map(|parent| crate::walk_boundary::simulated_volumes::simulate(parent, u64::MAX - 17));
    clean_old_caches_resolved(tiers, storage, storage, confirm)
}

/// Whether `storage` holds AFT's database (`aft.db`) or an owner marker that
/// names this very folder. Bulk deletions ask for this proof before treating a
/// folder as AFT's storage, so a mistyped `AFT_STORAGE_DIR` cannot point them
/// at someone else's data.
fn storage_root_proven(storage: &PinnedDir) -> io::Result<bool> {
    match storage.repair_read_file(std::ffi::OsStr::new("aft.db")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match storage.repair_read_file(std::ffi::OsStr::new(OWNER)) {
                Ok(marker) => {
                    let marker = read_marker_identity(marker)?;
                    Ok(
                        marker.owner == pinned_identity(storage)?
                            && !marker.registration.is_empty(),
                    )
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

pub use prune_legacy::{
    prune_legacy_stores, LegacyStore, LegacyStoreSkip, PruneLegacyError, PruneLegacyEvent,
    PruneLegacyReport,
};

/// Cache location is storage-wide: only the user base is resolved here, never
/// a harness override or a project redirect. Unavailable roots abort repair.
pub fn reclaim_configured_inactive_caches(
    tiers: &[crate::config_resolve::ConfigTier],
    storage: &Path,
) -> CleanupReport {
    let result = crate::config_resolve::resolve_config(tiers);
    if !result.errors.is_empty() {
        return CleanupReport {
            error: Some(format!("cache cleanup refused: {:?}", result.errors)),
            ..Default::default()
        };
    }
    // A fresh install has no storage root yet, so there is nothing to clean.
    // Report that rather than failing in the cleanup lease, and do not create
    // the root just to clean it.
    match fs::symlink_metadata(storage) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return CleanupReport::default();
        }
        _ => {}
    }
    let mut config = result.config;
    config.storage_dir = Some(storage.to_path_buf());
    let _cleanup_lease = match acquire_cache_cleanup_lease(storage) {
        Ok(lease) => lease,
        Err(error) => {
            return CleanupReport {
                error: Some(error.to_string()),
                ..Default::default()
            }
        }
    };
    if let Some(warning) = prepare_cache_root(&mut config) {
        return CleanupReport {
            error: Some(format!("cleanup refused: {}", warning.message)),
            reason: Some(warning.code),
            ..Default::default()
        };
    }
    remove_inactive_cache_roots(storage, &[cache_root(&config)])
}

fn open_daemon_storage_lock(root: &PinnedDir) -> io::Result<File> {
    let retention = root.repair_descendant(Path::new("retention"), true)?;
    retention.repair_coordination_file(std::ffi::OsStr::new(DAEMON_USE))
}

/// Cache-using processes share a durable-root lock before opening caches.
/// Daemon modules hold it even while waiting for their first bind. The kernel
/// releases it on exit; a leftover file does not indicate a running owner.
pub fn acquire_daemon_storage_lease(storage: &Path) -> io::Result<Arc<ProcessStorageLease>> {
    crate::private_storage::open_root(storage)?;
    acquire_pinned_storage_lease(pin_storage_root(storage)?)
}

/// Exclude another speech forwarder until managed quiescence or process exit.
pub(crate) fn acquire_speech_serving_lease(storage: &Path) -> io::Result<Arc<SpeechServingLease>> {
    let storage_use = acquire_daemon_storage_lease(storage)?;
    let root = pin_storage_root(storage)?;
    let identity = pinned_identity(&root)?;
    let mut leases = SPEECH_SERVING_LEASES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if leases.contains_key(&identity) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "speech forwarding already has an owner",
        ));
    }
    let retention = root.repair_descendant(Path::new("retention"), true)?;
    let file = retention.repair_coordination_file(std::ffi::OsStr::new("speech-serving.lock"))?;
    file.try_lock().map_err(storage_lock_error)?;
    let lease = Arc::new(SpeechServingLease {
        _file: file,
        _root: root,
        _storage_use: storage_use,
        identity,
    });
    leases.insert(identity, Arc::clone(&lease));
    Ok(lease)
}

/// Release only the anchor owned by the runtime that proved quiescence.
pub(crate) fn release_quiescent_speech_serving_lease(lease: &Arc<SpeechServingLease>) -> bool {
    let Some(leases) = SPEECH_SERVING_LEASES.get() else {
        return false;
    };
    let Ok(mut leases) = leases.lock() else {
        return false;
    };
    if !leases
        .get(&lease.identity)
        .is_some_and(|held| Arc::ptr_eq(held, lease))
    {
        return false;
    }
    leases.remove(&lease.identity);
    true
}

#[cfg(test)]
#[path = "gh_shim_speech/serving_tests.rs"]
mod speech_serving_tests;

fn acquire_pinned_storage_lease(root: PinnedDir) -> io::Result<Arc<ProcessStorageLease>> {
    let identity = pinned_identity(&root)?;
    let mut leases = PROCESS_STORAGE_LEASES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(lease) = leases.get(&identity).and_then(Weak::upgrade) {
        return Ok(lease);
    }
    // Config snapshots share one ownership handle per storage root. Weak
    // entries do not retain file descriptors after the last snapshot is gone.
    leases.retain(|_, lease| lease.strong_count() > 0);
    let file = open_daemon_storage_lock(&root)?;
    file.try_lock_shared().map_err(storage_lock_error)?;
    let lease = Arc::new(ProcessStorageLease {
        identity,
        _file: file,
        _root: root,
    });
    leases.insert(identity, Arc::downgrade(&lease));
    Ok(lease)
}

pub(crate) fn ensure_config_storage_lease(config: &mut crate::config::Config) -> io::Result<()> {
    let storage = crate::storage_path::storage_dir(config.storage_dir.as_deref());
    crate::private_storage::open_root(&storage)?;
    let root = pin_storage_root(&storage)?;
    let owner = pinned_identity(&root)?;
    if config
        .storage_lease
        .as_ref()
        .is_some_and(|lease| owner == lease.identity)
    {
        return Ok(());
    }
    config.storage_lease = Some(acquire_pinned_storage_lease(root)?);
    Ok(())
}

pub(crate) fn prepare_process_cache_root(
    config: &mut crate::config::Config,
) -> Option<crate::config_resolve::ConfigWarning> {
    if let Err(error) = ensure_config_storage_lease(config) {
        let storage = crate::storage_path::storage_dir(config.storage_dir.as_deref());
        record_split_durable(&storage, true);
        config.cache_unavailable = Some(error.to_string());
        config.cache_lease = None;
        return Some(unavailable_warning(&storage, &error.to_string()));
    }
    prepare_cache_root(config)
}

/// Hold exclusion across inventory, confirmation and deletion. A host-wide
/// process or connection-file check cannot identify which storage a daemon uses.
pub(crate) fn acquire_cache_cleanup_lease(storage: &Path) -> io::Result<File> {
    ensure_no_legacy_daemon()?;
    let root = pin_storage_root(storage)?;
    let file = open_daemon_storage_lock(&root)?;
    file.try_lock().map_err(|error| {
        let error = storage_lock_error(error);
        io::Error::new(
            error.kind(),
            format!(
                "stop the running daemon using storage root {} before cache cleanup: {error}",
                storage.display()
            ),
        )
    })?;
    Ok(file)
}

fn storage_lock_error(error: std::fs::TryLockError) -> io::Error {
    match error {
        std::fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "storage lease contention")
        }
        std::fs::TryLockError::Error(error) => error,
    }
}

#[cfg(test)]
// Unit tests supply process snapshots instead of inspecting unrelated daemons
// on the operator's machine; the same classifier and cleanup refusal still run.
// An empty snapshot means the test supplied none, and the scan is skipped.
thread_local! { static LEGACY_DAEMON_PROCESSES: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) }; }

/// A process snapshot has one process per line:
/// `pid<TAB>executable<TAB>arguments`.
///
/// `executable` is the program's path or file name exactly as the operating
/// system records it, never a word split from a command line: a daemon at
/// `/Users/Jane Doe/.../subc` or `"C:\Program Files\AFT\subc.exe"` would
/// otherwise read as `Jane` or `"C:\Program`. A line without the second tab
/// means the process's arguments could not be read.
struct ListedProcess<'a> {
    pid: u32,
    executable: &'a str,
    args: Option<&'a str>,
}

/// `None` for a line that does not name a process.
fn listed_process(line: &str) -> Option<ListedProcess<'_>> {
    let mut fields = line.trim_end_matches('\r').splitn(3, '\t');
    let pid = fields.next()?.trim().parse().ok()?;
    let executable = fields.next()?.trim();
    Some(ListedProcess {
        pid,
        executable,
        args: fields.next(),
    })
}

/// Join the two `ps` queries into a snapshot: `ps -axo pid=,comm=` names each
/// process's executable as the whole rest of its line (so a path with spaces
/// stays whole), and `ps -axo pid=,args=` gives its command line. The
/// executable query defines the snapshot; a process missing from the
/// arguments query (it exited in between) has unknown arguments.
// Only the Unix scan runs two queries; tests exercise the join everywhere.
#[cfg_attr(all(windows, not(test)), allow(dead_code))]
fn compose_ps_snapshot(executables: &str, command_lines: &str) -> String {
    let flatten = |text: &str| text.replace('\t', " ");
    let args = command_lines
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            Some((pid.parse::<u32>().ok()?, rest.trim()))
        })
        .collect::<HashMap<_, _>>();
    let mut snapshot = String::new();
    for line in executables.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (pid, executable) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match pid.parse::<u32>() {
            Ok(pid) => {
                snapshot.push_str(&format!("{pid}\t{}", flatten(executable.trim())));
                if let Some(args) = args.get(&pid) {
                    snapshot.push('\t');
                    snapshot.push_str(&flatten(args));
                }
            }
            // Kept as is so the classifier reports the snapshot as unreadable.
            Err(_) => snapshot.push_str(line),
        }
        snapshot.push('\n');
    }
    snapshot
}

/// Whether the snapshot lists a subc daemon other than this process.
///
/// Every doubt refuses: a snapshot that does not list this process (the scan
/// failed partway, returned nothing, or could not see every process), a line
/// that names no process, a process whose executable is unknown, or an aft
/// executable whose arguments are unknown cannot show that no older daemon is
/// running. Those are errors, and cleanup holds.
fn legacy_daemon_present(processes: &str, current_pid: u32) -> io::Result<bool> {
    let cannot_tell = |why: String| {
        io::Error::other(format!(
            "legacy_daemon_upgrade_window: cannot tell whether an older subc daemon is running ({why}); cleanup refused"
        ))
    };
    let mut lists_this_process = false;
    let mut present = false;
    for line in processes.lines().filter(|line| !line.trim().is_empty()) {
        let process = listed_process(line)
            .ok_or_else(|| cannot_tell(format!("unreadable process scan line {line:?}")))?;
        if process.pid == current_pid {
            lists_this_process = true;
            continue;
        }
        if process.executable.is_empty() {
            return Err(cannot_tell(format!(
                "the executable of process {} is unknown",
                process.pid
            )));
        }
        // Windows file names are case-insensitive; elsewhere a differently
        // cased name only makes the check stricter.
        let name = Path::new(process.executable)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(process.executable)
            .to_ascii_lowercase();
        if matches!(name.as_str(), "subc" | "subc.exe") {
            present = true;
        } else if matches!(name.as_str(), "aft" | "aft.exe" | "ck-aft" | "ck-aft.exe") {
            let args = process.args.ok_or_else(|| {
                cannot_tell(format!(
                    "the arguments of {} process {} are unknown",
                    name, process.pid
                ))
            })?;
            present |= args
                .split_whitespace()
                .any(|word| word == "--subc" || word.starts_with("--subc="));
        }
    }
    if !lists_this_process {
        return Err(cannot_tell(
            "the process scan did not list this process".to_string(),
        ));
    }
    Ok(present)
}

/// List every process in the snapshot format `legacy_daemon_present` reads.
fn legacy_process_query(label: &str, timeout: Duration) -> io::Result<String> {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("powershell.exe");
        // `Name` is the executable's file name. Tabs separate the fields and
        // line breaks inside a command line are flattened, so each process is
        // one line; a command line Windows does not disclose is left out, which
        // marks its arguments unknown.
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process | ForEach-Object { if ($null -eq $_.CommandLine) { \"{0}`t{1}\" -f $_.ProcessId, $_.Name } else { \"{0}`t{1}`t{2}\" -f $_.ProcessId, $_.Name, ($_.CommandLine -replace '[\\r\\n\\t]+', ' ') } }",
        ]);
        legacy_process_snapshot(&mut command, label, timeout)
    }
    #[cfg(not(windows))]
    {
        let mut executables = std::process::Command::new("ps");
        executables.args(["-axo", "pid=,comm="]);
        let executables =
            legacy_process_snapshot(&mut executables, &format!("{label} (executables)"), timeout)?;
        let mut command_lines = std::process::Command::new("ps");
        command_lines.args(["-axo", "pid=,args="]);
        let command_lines =
            legacy_process_snapshot(&mut command_lines, &format!("{label} (arguments)"), timeout)?;
        Ok(compose_ps_snapshot(&executables, &command_lines))
    }
}

fn legacy_process_snapshot(
    command: &mut std::process::Command,
    label: &str,
    timeout: Duration,
) -> io::Result<String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let deadline = Instant::now() + timeout;
    let mut child = command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{label}: cannot start process scan: {error}; cleanup refused"),
        )
    })?;
    let mut stdout = child.stdout.take().expect("piped process snapshot stdout");
    let (sender, reader) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    let timed_out = |waiting_for: &str| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{label}: deadline {timeout:?} exceeded waiting for {waiting_for}; cleanup refused"
            ),
        )
    };
    let mut exited = false;
    let result = (|| {
        // Reading on a worker avoids blocking forever on a scan or inherited
        // stdout writer. Stdout EOF and child exit share the same deadline.
        let bytes = reader
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| timed_out("process snapshot stdout EOF"))??;
        loop {
            if let Some(status) = child.try_wait()? {
                exited = true;
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "{label} exited with {status}; cleanup refused"
                    )));
                }
                return Ok(String::from_utf8_lossy(&bytes).into_owned());
            }
            if Instant::now() >= deadline {
                return Err(timed_out("process snapshot exit"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() && !exited {
        // Terminate the scan's process group before reaping: a descendant can
        // retain stdout even after the direct child has exited.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let reap_deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait()?.is_none() {
            if Instant::now() >= reap_deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, format!("{label}: deadline 2s exceeded waiting for terminated process snapshot to be reaped; cleanup refused")));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    result
}

fn ensure_no_legacy_daemon() -> io::Result<()> {
    // Upgrade-window protection: older daemons do not hold the durable-root
    // lock and cannot be excluded by it. Keep this host-wide refusal for this release.
    #[cfg(test)]
    let processes = LEGACY_DAEMON_PROCESSES.with(|processes| processes.borrow().clone());
    #[cfg(test)]
    if processes.is_empty() {
        return Ok(());
    }
    #[cfg(not(test))]
    let processes = legacy_process_query("legacy-daemon process scan", Duration::from_secs(10))?;
    if legacy_daemon_present(&processes, std::process::id())? {
        return Err(io::Error::other(
            "legacy_daemon_upgrade_window: stop the running subc daemon before cache cleanup",
        ));
    }
    Ok(())
}

/// `aft cache prune-legacy`: deletes the index stores AFT used before version
/// 0.64. From 0.64 every index lives in `data/` and nothing opens the old
/// stores, so this runs while AFT daemons keep running. It never deletes on its
/// own initiative; the command is the only caller.
///
/// Safety rules, in the order a run applies them:
/// - The storage root must prove it is AFT's (`aft.db` or its owner marker).
///   An external `storage.cache_dir` root must carry an owner marker naming
///   that storage root and be listed in the storage root's record of the cache
///   roots it prepared (`retention/cache-roots.json`).
/// - Only the fixed folder names below are candidates, plus each child of
///   `semantic/` except the live `semantic/models/`. Each is opened without
///   following symlinks and must be a real folder on the root's own device and
///   mount; a symlink, a mount point or a folder on another device is skipped.
/// - A bounded walk sizes each candidate and refuses one that holds a mount, a
///   hard-linked file or a special file within the part it read.
/// - With `--yes`, each candidate is first renamed into
///   `<root>/.prune-legacy-<time>-<pid>/` on the same filesystem, and only that
///   moved copy is deleted. A folder that cannot be renamed (on Windows, a
///   file open inside it) is skipped, never deleted in place.
/// - Deletion runs in bounded passes that never follow links or cross mounts.
///   A run that stops early leaves the moved folder behind, and the next run
///   deletes every `.prune-legacy-*` leftover.
mod prune_legacy {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{
        known_harness, marker_matches_pinned, paths_overlap, pin_storage_root, pinned_identity,
        storage_root_proven, DAEMON_USE,
    };
    use crate::bash_background::persistence::{PinnedDir, RepairBudget, RepairLimitExceeded};

    /// The stores AFT kept directly under a storage or cache root before 0.64.
    /// `semantic/` is handled apart (see `SEMANTIC_DIR`).
    pub(super) const LEGACY_STORE_DIRS: [&str; 8] = [
        "views",
        "blobs",
        "callgraph",
        "index",
        "symbols",
        "artifact-owners",
        "migration",
        "semantic-batch-leases-v1",
    ];
    /// Each harness folder (`opencode/`, `pi/`, ...) once held its own call
    /// graph store under this name. Nothing else in a harness folder is touched.
    pub(super) const LEGACY_HARNESS_STORE_DIR: &str = "callgraph";
    /// `semantic/` mixes old per-project indexes with this live folder: the
    /// embedding and reranker models AFT downloads (`FASTEMBED_CACHE_DIR`
    /// and `semantic.local_cache_dir` point here). It is kept, so every other
    /// child of `semantic/` is a candidate on its own instead of the folder.
    pub(super) const SEMANTIC_DIR: &str = "semantic";
    pub(super) const SEMANTIC_MODELS_DIR: &str = "models";
    /// Name prefix of the folder a run moves stores into before deleting them.
    pub(super) const ASIDE_PREFIX: &str = ".prune-legacy-";
    /// Held exclusively for a whole `--yes` run so two runs never delete the
    /// same moved folder at once. Lives in `retention/` with the other
    /// storage-root locks.
    const RUN_LOCK: &str = "prune-legacy.lock";
    /// Most entries one size walk reads before it reports a lower bound.
    const SIZE_ENTRY_LIMIT: usize = 500_000;
    /// Longest one size walk runs before it reports a lower bound.
    const SIZE_TIME_LIMIT: Duration = Duration::from_secs(20);
    /// Deepest folder nesting the deletion walk accepts (see `RepairBudget`).
    const MAX_DEPTH: usize = 64;
    /// Most bounded deletion passes over one folder. Each pass removes up to
    /// one `RepairBudget` worth of entries, and a pass that removes nothing
    /// ends the deletion, so this only caps a pathological tree.
    const MAX_DELETE_PASSES: usize = 10_000;

    #[cfg(test)]
    thread_local! {
        /// Folder names whose rename aside fails, as it does on Windows while
        /// a file inside the folder is open.
        pub(super) static RENAME_REFUSED: std::cell::RefCell<Vec<OsString>> =
            const { std::cell::RefCell::new(Vec::new()) };
        /// Stop a `--yes` run right after the moves, as a crash would.
        pub(super) static STOP_AFTER_MOVE: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    /// One folder a run found.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct LegacyStore {
        /// Where the folder was found, before any move.
        pub path: PathBuf,
        /// Bytes counted by the bounded size walk.
        pub bytes: u64,
        /// False when the size walk stopped at its bound, so `bytes` is only
        /// a lower bound.
        pub complete: bool,
        /// A `.prune-legacy-*` folder that an earlier, interrupted run left.
        pub leftover: bool,
    }

    /// A folder the run left in place, with the reason in plain words.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct LegacyStoreSkip {
        pub path: PathBuf,
        pub reason: String,
    }

    #[derive(Debug, Default)]
    pub struct PruneLegacyReport {
        /// Every root the run examined, the storage root first. Empty when
        /// the storage root does not exist.
        pub roots: Vec<PathBuf>,
        /// Folders a dry run would delete, or a `--yes` run tried to delete.
        pub stores: Vec<LegacyStore>,
        /// Folders a `--yes` run deleted.
        pub removed: Vec<LegacyStore>,
        /// Folders (or a whole cache root) the run left in place.
        pub skipped: Vec<LegacyStoreSkip>,
        /// Things the run could not check, such as an unreadable config.
        pub notes: Vec<String>,
    }

    /// Reported while a `--yes` run works, so a long deletion shows progress.
    #[derive(Debug)]
    pub enum PruneLegacyEvent<'a> {
        Deleting(&'a LegacyStore),
        Removed(&'a LegacyStore),
        Skipped(&'a LegacyStoreSkip),
    }

    #[derive(Debug)]
    pub enum PruneLegacyError {
        /// The run stopped before changing anything; the text says why.
        Refused(String),
        /// An unexpected I/O failure.
        Failed(io::Error),
    }

    impl From<io::Error> for PruneLegacyError {
        fn from(error: io::Error) -> Self {
            Self::Failed(error)
        }
    }

    /// One root to clean: the storage root or the external cache root.
    struct Root {
        dir: PinnedDir,
        /// Identities of the directories kept at the top of this root. A
        /// deletion walk that meets one of them (through a bind mount or an
        /// aliased directory) stops instead of deleting it.
        protected: Vec<(u64, u64)>,
        candidates: Vec<Candidate>,
    }

    /// A folder that passed every check and may be moved and deleted.
    struct Candidate {
        store: LegacyStore,
        /// The folder that holds it: the root, or a harness folder.
        parent: PinnedDir,
        name: OsString,
        identity: (u64, u64),
    }

    /// Lists (and with `yes`, deletes) the legacy stores under `storage` and
    /// under the external cache root `storage.cache_dir` from `tiers`.
    pub fn prune_legacy_stores(
        tiers: &[crate::config_resolve::ConfigTier],
        storage: &Path,
        yes: bool,
        on_event: &mut dyn FnMut(PruneLegacyEvent<'_>),
    ) -> Result<PruneLegacyReport, PruneLegacyError> {
        let mut report = PruneLegacyReport::default();
        match fs::symlink_metadata(storage) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let storage_dir = pin_storage_root(storage).map_err(|error| {
            PruneLegacyError::Refused(format!(
                "Cannot open the AFT storage folder {}: {error}",
                storage.display()
            ))
        })?;
        if !storage_root_proven(&storage_dir)? {
            return Err(PruneLegacyError::Refused(format!(
                "{} does not look like an AFT storage folder: it has no aft.db and no AFT owner marker.",
                storage.display()
            )));
        }
        // A dry run only reads, so it takes no lock and writes nothing.
        let _locks = if yes {
            Some(lock_run(&storage_dir, storage)?)
        } else {
            None
        };
        let mut roots = vec![storage_dir.repair_clone()?];
        if let Some(cache) = external_cache_root(tiers, storage, &storage_dir, &mut report)? {
            roots.push(cache);
        }
        let mut planned = Vec::new();
        for dir in roots {
            report.roots.push(dir.path().to_path_buf());
            planned.push(plan_root(dir, &mut report)?);
        }
        if !yes {
            return Ok(report);
        }
        for root in planned {
            delete_root(root, &mut report, on_event)?;
        }
        Ok(report)
    }

    /// Takes the run lock and a shared hold on the storage root's daemon lock.
    /// Daemons share that lock too, so AFT keeps running; an old-cache cleanup
    /// takes it exclusively and so never deletes the same folders at once.
    fn lock_run(
        storage_dir: &PinnedDir,
        storage: &Path,
    ) -> Result<Vec<fs::File>, PruneLegacyError> {
        let retention = storage_dir.repair_descendant(Path::new("retention"), true)?;
        let busy = || {
            PruneLegacyError::Refused(format!(
                "Another cleanup is running on {}. Try again when it has finished.",
                storage.display()
            ))
        };
        let run = retention.repair_coordination_file(OsStr::new(RUN_LOCK))?;
        match run.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(busy()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let daemon = retention.repair_coordination_file(OsStr::new(DAEMON_USE))?;
        match daemon.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(busy()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        Ok(vec![run, daemon])
    }

    /// The `aft/` folder under a configured `storage.cache_dir`, once it is
    /// proven to belong to `storage`. Anything that prevents checking it is
    /// reported, never treated as "nothing there".
    fn external_cache_root(
        tiers: &[crate::config_resolve::ConfigTier],
        storage: &Path,
        storage_dir: &PinnedDir,
        report: &mut PruneLegacyReport,
    ) -> io::Result<Option<PinnedDir>> {
        let resolved = crate::config_resolve::resolve_config(tiers);
        if !resolved.errors.is_empty() {
            report.notes.push(
                "Your AFT config has errors, so a cache folder set by storage.cache_dir was not checked."
                    .to_string(),
            );
            return Ok(None);
        }
        if let Some(reason) = &resolved.config.cache_config_unknown {
            report.notes.push(format!(
                "AFT could not tell whether storage.cache_dir is set, so no cache folder was checked: {reason}"
            ));
            return Ok(None);
        }
        let Some(cache_dir) = resolved.config.cache_dir else {
            return Ok(None);
        };
        let root = cache_dir.join("aft");
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        let skip = |report: &mut PruneLegacyReport, reason: String| {
            report.skipped.push(LegacyStoreSkip {
                path: root.clone(),
                reason,
            });
            Ok(None)
        };
        if !root.is_absolute() {
            return skip(
                report,
                "storage.cache_dir is not an absolute path, so this folder was not checked."
                    .to_string(),
            );
        }
        let dir = match pin_storage_root(&root) {
            Ok(dir) => dir,
            Err(error) => return skip(report, format!("It could not be opened: {error}")),
        };
        if pinned_identity(&dir)? == pinned_identity(storage_dir)? {
            // The same folder as the storage root, which is already checked.
            return Ok(None);
        }
        if paths_overlap(&root, storage) {
            return skip(
                report,
                "It is inside the storage folder or contains it, so it was not checked separately."
                    .to_string(),
            );
        }
        if let Err(error) = marker_matches_pinned(&dir, storage_dir) {
            return skip(
                report,
                format!("It is not proven to belong to this AFT storage folder: {error}"),
            );
        }
        Ok(Some(dir))
    }

    /// Finds and checks every candidate under one root. Adds each admitted
    /// folder to `report.stores` and each refused one to `report.skipped`.
    fn plan_root(dir: PinnedDir, report: &mut PruneLegacyReport) -> io::Result<Root> {
        let mut protected = vec![dir.directory_identity()?];
        // (parent, name, leftover) for every folder that may be a candidate.
        let mut found: Vec<(PinnedDir, OsString, bool)> = Vec::new();
        let mut names = dir.repair_names(&mut RepairBudget::new())?;
        names.sort();
        for name in names {
            let text = name.to_str();
            if text == Some(SEMANTIC_DIR) {
                match dir.repair_child(&name) {
                    Ok(semantic) => {
                        protected.push(semantic.directory_identity()?);
                        let mut inner_names = semantic.repair_names(&mut RepairBudget::new())?;
                        inner_names.sort();
                        for inner in inner_names {
                            if inner == OsStr::new(SEMANTIC_MODELS_DIR) {
                                if let Ok(models) = semantic.repair_child(&inner) {
                                    protected.push(models.directory_identity()?);
                                }
                                continue;
                            }
                            // Old indexes are per-project folders. A plain
                            // file here is no store, so it stays unmentioned;
                            // a link is still listed and refused.
                            let plain_file = fs::symlink_metadata(semantic.path().join(&inner))
                                .is_ok_and(|metadata| metadata.is_file());
                            if !plain_file {
                                found.push((semantic.repair_clone()?, inner, false));
                            }
                        }
                    }
                    // Not a real folder: the check below names the reason.
                    Err(_) => found.push((dir.repair_clone()?, name, false)),
                }
                continue;
            }
            if text.is_some_and(|text| LEGACY_STORE_DIRS.contains(&text)) {
                found.push((dir.repair_clone()?, name, false));
                continue;
            }
            if text.is_some_and(|text| text.starts_with(ASIDE_PREFIX)) {
                found.push((dir.repair_clone()?, name, true));
                continue;
            }
            // Everything else at the top of the root is kept. Remember the
            // identity of each kept folder so no deletion walk can reach it.
            let Ok(child) = dir.repair_child(&name) else {
                continue;
            };
            protected.push(child.directory_identity()?);
            if !text.is_some_and(known_harness) {
                continue;
            }
            for inner in child.repair_names(&mut RepairBudget::new())? {
                if inner == OsStr::new(LEGACY_HARNESS_STORE_DIR) {
                    found.push((child.repair_clone()?, inner, false));
                } else if let Ok(kept) = child.repair_child(&inner) {
                    protected.push(kept.directory_identity()?);
                }
            }
        }
        let mut candidates = Vec::new();
        for (parent, name, leftover) in found {
            let path = parent.path().join(&name);
            match check_candidate(&dir, &parent, &name, &protected) {
                Ok(None) => {}
                Ok(Some((identity, bytes, complete))) => {
                    let store = LegacyStore {
                        path,
                        bytes,
                        complete,
                        leftover,
                    };
                    report.stores.push(store.clone());
                    candidates.push(Candidate {
                        store,
                        parent,
                        name,
                        identity,
                    });
                }
                Err(reason) => report.skipped.push(LegacyStoreSkip { path, reason }),
            }
        }
        Ok(Root {
            dir,
            protected,
            candidates,
        })
    }

    /// Opens `name` in `parent` without following a link and sizes it. `None`
    /// when it is gone; `Err` holds the reason it must be left in place.
    fn check_candidate(
        root: &PinnedDir,
        parent: &PinnedDir,
        name: &OsStr,
        protected: &[(u64, u64)],
    ) -> Result<Option<((u64, u64), u64, bool)>, String> {
        let folder = match parent.repair_child(name) {
            Ok(folder) => folder,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if leaf_error(&error) => {
                // Looking up the path's own metadata (without following a
                // link) only names the entry's kind for the message; the
                // entry itself is never opened through the link.
                let link = fs::symlink_metadata(parent.path().join(name))
                    .is_ok_and(|metadata| metadata.file_type().is_symlink());
                return Err(if link {
                    "It is a symbolic link. This command never follows links.".to_string()
                } else {
                    "It is not a folder.".to_string()
                });
            }
            Err(error) => return Err(format!("It could not be opened: {error}")),
        };
        if folder.repair_check_boundary(root, protected).is_err() {
            return Err(
                "It is on another disk or is a mount point, so it was left alone.".to_string(),
            );
        }
        let identity = folder
            .directory_identity()
            .map_err(|error| format!("It could not be checked: {error}"))?;
        let mut walk = SizeWalk {
            bytes: 0,
            entries: 0,
            deadline: Instant::now() + SIZE_TIME_LIMIT,
            budget: RepairBudget::new(),
        };
        match walk.measure(&folder, root, protected, 0) {
            Ok(()) => Ok(Some((identity, walk.bytes, true))),
            Err(SizeStop::Bound) => Ok(Some((identity, walk.bytes, false))),
            Err(SizeStop::Refuse(reason)) => Err(reason),
        }
    }

    /// Whether opening an entry as a folder failed because it is not one: a
    /// file, or a symlink that a no-follow open refuses.
    fn leaf_error(error: &io::Error) -> bool {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return true;
        }
        error.kind() == io::ErrorKind::NotADirectory
    }

    fn budget_stop(error: &io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(|inner| inner.is::<RepairLimitExceeded>())
    }

    enum SizeStop {
        /// The walk reached its entry or time bound; the count is a lower bound.
        Bound,
        /// The folder holds something this command must not delete.
        Refuse(String),
    }

    /// A bounded size walk that also refuses what deletion must not touch.
    /// It checks its bound before every entry, not after a whole folder.
    struct SizeWalk {
        bytes: u64,
        entries: usize,
        deadline: Instant,
        budget: RepairBudget,
    }

    impl SizeWalk {
        fn measure(
            &mut self,
            folder: &PinnedDir,
            root: &PinnedDir,
            protected: &[(u64, u64)],
            depth: usize,
        ) -> Result<(), SizeStop> {
            if depth > MAX_DEPTH {
                return Err(SizeStop::Refuse(format!(
                    "It is nested more than {MAX_DEPTH} folders deep."
                )));
            }
            if folder.repair_check_boundary(root, protected).is_err() {
                return Err(SizeStop::Refuse(format!(
                    "It contains {}, which is a mount point or on another disk.",
                    folder.path().display()
                )));
            }
            let names = match folder.repair_names(&mut self.budget) {
                Ok(names) => names,
                Err(error) if budget_stop(&error) => return Err(SizeStop::Bound),
                Err(error) => {
                    return Err(SizeStop::Refuse(format!(
                        "{} could not be listed: {error}",
                        folder.path().display()
                    )))
                }
            };
            for name in names {
                self.entries += 1;
                if self.entries > SIZE_ENTRY_LIMIT || Instant::now() >= self.deadline {
                    return Err(SizeStop::Bound);
                }
                match folder.repair_child(&name) {
                    Ok(child) => self.measure(&child, root, protected, depth + 1)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) if leaf_error(&error) => match folder.repair_read_file(&name) {
                        Ok(file) => {
                            let length = file.metadata().map(|metadata| metadata.len());
                            self.bytes = self.bytes.saturating_add(length.unwrap_or(0));
                        }
                        // A symlink is deleted as a link; its target is never
                        // opened or counted.
                        Err(error) if leaf_error(&error) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(SizeStop::Refuse(format!(
                                "It contains {}, which has more than one hard link or is not a regular file ({error}).",
                                folder.path().join(&name).display()
                            )))
                        }
                    },
                    Err(error) => {
                        return Err(SizeStop::Refuse(format!(
                            "{} could not be opened: {error}",
                            folder.path().join(&name).display()
                        )))
                    }
                }
            }
            Ok(())
        }
    }

    /// Moves every admitted folder of one root aside, then deletes what moved
    /// and every leftover from earlier runs.
    fn delete_root(
        root: Root,
        report: &mut PruneLegacyReport,
        on_event: &mut dyn FnMut(PruneLegacyEvent<'_>),
    ) -> io::Result<()> {
        let Root {
            dir,
            protected,
            candidates,
        } = root;
        let (leftovers, stores): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|candidate| candidate.store.leftover);
        // Each moved folder is deleted from inside the aside folder:
        // (store, its name there, its identity).
        let mut moved: Vec<(LegacyStore, OsString, (u64, u64))> = Vec::new();
        let mut aside: Option<(PinnedDir, OsString)> = None;
        for (index, candidate) in stores.into_iter().enumerate() {
            if aside.is_none() {
                aside = Some(create_aside(&dir)?);
            }
            let (aside_dir, _) = aside.as_ref().expect("aside folder was just created");
            // A harness store keeps its harness in its moved name so two
            // `callgraph` folders never collide.
            let target = OsString::from(format!(
                "{index}-{}",
                candidate
                    .store
                    .path
                    .strip_prefix(dir.path())
                    .unwrap_or(&candidate.store.path)
                    .to_string_lossy()
                    .replace(['/', '\\'], "-")
            ));
            match move_aside(&candidate, aside_dir, &target) {
                Ok(()) => moved.push((candidate.store, target, candidate.identity)),
                Err(error) => {
                    let skip = LegacyStoreSkip {
                        path: candidate.store.path,
                        reason: format!(
                            "It could not be moved aside, so nothing in it was deleted: {error}"
                        ),
                    };
                    on_event(PruneLegacyEvent::Skipped(&skip));
                    report.skipped.push(skip);
                }
            }
        }
        #[cfg(test)]
        if STOP_AFTER_MOVE.with(|stop| stop.get()) {
            return Ok(());
        }
        if let Some((aside_dir, aside_name)) = &aside {
            for (store, name, identity) in moved {
                on_event(PruneLegacyEvent::Deleting(&store));
                match delete_in_passes(aside_dir, &name, identity, &protected) {
                    Ok(()) => {
                        on_event(PruneLegacyEvent::Removed(&store));
                        report.removed.push(store);
                    }
                    Err(error) => {
                        let skip = LegacyStoreSkip {
                            reason: format!(
                                "It was moved to {} but not fully deleted: {error}. The next run tries again.",
                                aside_dir.path().join(&name).display()
                            ),
                            path: store.path,
                        };
                        on_event(PruneLegacyEvent::Skipped(&skip));
                        report.skipped.push(skip);
                    }
                }
            }
            // The aside folder is empty unless something above was kept.
            let aside_identity = aside_dir.directory_identity()?;
            if aside_dir.repair_names(&mut RepairBudget::new())?.is_empty() {
                delete_in_passes(&dir, aside_name, aside_identity, &protected)?;
            }
        }
        for leftover in leftovers {
            on_event(PruneLegacyEvent::Deleting(&leftover.store));
            match delete_in_passes(
                &leftover.parent,
                &leftover.name,
                leftover.identity,
                &protected,
            ) {
                Ok(()) => {
                    on_event(PruneLegacyEvent::Removed(&leftover.store));
                    report.removed.push(leftover.store);
                }
                Err(error) => {
                    let skip = LegacyStoreSkip {
                        path: leftover.store.path,
                        reason: format!(
                            "It was not fully deleted: {error}. The next run tries again."
                        ),
                    };
                    on_event(PruneLegacyEvent::Skipped(&skip));
                    report.skipped.push(skip);
                }
            }
        }
        Ok(())
    }

    /// Creates `<root>/.prune-legacy-<unix-millis>-<pid>[-n]/`.
    fn create_aside(root: &PinnedDir) -> io::Result<(PinnedDir, OsString)> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let base = format!("{ASIDE_PREFIX}{millis}-{}", std::process::id());
        for attempt in 0..16 {
            let name = OsString::from(if attempt == 0 {
                base.clone()
            } else {
                format!("{base}-{attempt}")
            });
            match root.repair_create_dir(&name) {
                Ok(created) => return Ok((created, name)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("every {base}-* name is taken"),
        ))
    }

    /// Renames one folder into the aside folder with a single rename that
    /// never replaces anything, after checking it is still the folder sized
    /// earlier.
    fn move_aside(candidate: &Candidate, aside: &PinnedDir, target: &OsStr) -> io::Result<()> {
        #[cfg(test)]
        if RENAME_REFUSED.with(|names| names.borrow().iter().any(|name| *name == candidate.name)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a file inside it is open (simulated)",
            ));
        }
        let entry = candidate.parent.repair_entry_for_rename(&candidate.name)?;
        if entry.identity()? != candidate.identity {
            return Err(io::Error::other("it changed while it was being checked"));
        }
        candidate
            .parent
            .repair_rename_entry_to(&candidate.name, &entry, aside, target)
    }

    /// Deletes folder `name` inside `parent` in bounded passes. Each pass reads
    /// at most one `RepairBudget` of entries, never follows a link and stops
    /// at a mount or a protected folder. Passes repeat while they make
    /// progress, so a large store is deleted in one run.
    fn delete_in_passes(
        parent: &PinnedDir,
        name: &OsStr,
        identity: (u64, u64),
        protected: &[(u64, u64)],
    ) -> io::Result<()> {
        let mut warnings = Vec::new();
        for _ in 0..MAX_DELETE_PASSES {
            let mut removed = 0;
            match parent.repair_remove_child_with_budget(
                name,
                identity,
                protected,
                &mut removed,
                &mut warnings,
                &mut RepairBudget::new(),
            ) {
                Ok(()) => return Ok(()),
                Err(error) if removed > 0 && budget_stop(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other(RepairLimitExceeded))
    }
}

#[cfg(test)]
mod prune_legacy_tests {
    use super::prune_legacy::{ASIDE_PREFIX, RENAME_REFUSED, STOP_AFTER_MOVE};
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    /// Every legacy entry the fixture plants, relative to the storage root.
    const LEGACY: [&str; 12] = [
        "views/view-1/derived.sqlite",
        "blobs/ab/cdef",
        "callgraph/key-1/build-breaker.sqlite",
        "index/key-1/cache.bin",
        "semantic/key-1/semantic.bin",
        "semantic/key-2/semantic.bin",
        "symbols/key-1/symbols.sqlite",
        "artifact-owners/owner.json",
        "migration/state.json",
        "semantic-batch-leases-v1/lease.json",
        "opencode/callgraph/key-1/graph.sqlite",
        "pi/callgraph/key-2/graph.sqlite",
    ];
    /// The folders a run must find for `LEGACY`, written out by hand so the
    /// expectation does not reuse the code's own list.
    const LEGACY_FOLDERS: [&str; 12] = [
        "artifact-owners",
        "blobs",
        "callgraph",
        "index",
        "migration",
        "opencode/callgraph",
        "pi/callgraph",
        "semantic-batch-leases-v1",
        "semantic/key-1",
        "semantic/key-2",
        "symbols",
        "views",
    ];
    /// What a run must leave byte-identical.
    const PROTECTED: [&str; 15] = [
        "aft.db",
        "aft.db-wal",
        "cache-keys.json",
        "data/store/main.sqlite",
        "inspect/key-1/inspect.sqlite",
        "retention/daemon-use.lock",
        "root-identities/key-1.json",
        "backups/session/stack",
        "opencode/inspect/key-1/inspect.sqlite",
        "opencode/sessions/one.json",
        "pi/settings.json",
        "semantic/models/models--Qdrant--all-MiniLM-L6-v2-onnx/model.onnx",
        "semantic/notes.txt",
        "unknown-folder/views/keep",
        "views.txt",
    ];

    struct Fixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
        storage: PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        // Canonical, so test hooks keyed by path match the pinned spelling
        // (macOS reaches temp folders through /var -> /private/var).
        let base = fs::canonicalize(temp.path()).unwrap();
        // On Windows canonical paths carry the verbatim `\\?\` prefix, and
        // the command reports and keys paths in their plain spelling.
        #[cfg(windows)]
        let base = crate::windows_path::normalize_windows_path(&base);
        let storage = base.join("storage");
        for relative in LEGACY.iter().chain(&PROTECTED) {
            let path = storage.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("contents of {relative}")).unwrap();
        }
        Fixture {
            _temp: temp,
            base,
            storage,
        }
    }

    /// Every file and folder under `root` with its bytes (folders map to
    /// nothing), read without following links.
    fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let kind = fs::symlink_metadata(&path).unwrap().file_type();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if kind.is_dir() {
                    pending.push(path);
                    found.insert(relative, None);
                } else if kind.is_symlink() {
                    found.insert(relative, Some(b"<link>".to_vec()));
                } else {
                    found.insert(relative, Some(fs::read(&path).unwrap()));
                }
            }
        }
        found
    }

    fn protected_bytes(storage: &Path) -> Vec<(String, Vec<u8>)> {
        PROTECTED
            .iter()
            .map(|relative| {
                (
                    relative.to_string(),
                    fs::read(storage.join(relative)).unwrap(),
                )
            })
            .collect()
    }

    fn legacy_folders(storage: &Path) -> Vec<PathBuf> {
        let mut paths = LEGACY_FOLDERS
            .iter()
            .map(|relative| storage.join(relative))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn run(storage: &Path, yes: bool) -> PruneLegacyReport {
        run_with_tiers(&[], storage, yes)
    }

    fn run_with_tiers(
        tiers: &[crate::config_resolve::ConfigTier],
        storage: &Path,
        yes: bool,
    ) -> PruneLegacyReport {
        match prune_legacy_stores(tiers, storage, yes, &mut |_| {}) {
            Ok(report) => report,
            Err(PruneLegacyError::Refused(reason)) => panic!("refused: {reason}"),
            Err(PruneLegacyError::Failed(error)) => panic!("failed: {error}"),
        }
    }

    fn found_paths(stores: &[LegacyStore]) -> Vec<PathBuf> {
        let mut paths = stores
            .iter()
            .map(|store| store.path.clone())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn asides(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(ASIDE_PREFIX))
            })
            .collect()
    }

    #[test]
    fn prune_legacy_dry_run_lists_exactly_the_legacy_stores_and_changes_nothing() {
        let fixture = fixture();
        let before = tree(&fixture.base);
        let report = run(&fixture.storage, false);
        assert_eq!(tree(&fixture.base), before, "a dry run changed the disk");
        assert_eq!(
            found_paths(&report.stores),
            legacy_folders(&fixture.storage)
        );
        assert!(report
            .stores
            .iter()
            .all(|store| store.complete && !store.leftover));
        let views = report
            .stores
            .iter()
            .find(|store| store.path.ends_with("views"))
            .unwrap();
        assert_eq!(
            views.bytes,
            "contents of views/view-1/derived.sqlite".len() as u64
        );
        assert!(report.removed.is_empty());
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        assert_eq!(report.roots, vec![fixture.storage.clone()]);
    }

    #[test]
    fn prune_legacy_yes_removes_the_stores_and_keeps_everything_else_byte_identical() {
        let fixture = fixture();
        let kept = protected_bytes(&fixture.storage);
        let mut events = Vec::new();
        let report = prune_legacy_stores(&[], &fixture.storage, true, &mut |event| {
            events.push(format!("{event:?}"));
        })
        .unwrap_or_else(|error| panic!("{error:?}"));
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(
            found_paths(&report.removed),
            legacy_folders(&fixture.storage)
        );
        for path in legacy_folders(&fixture.storage) {
            assert!(
                fs::symlink_metadata(&path).is_err(),
                "{} survived",
                path.display()
            );
        }
        assert_eq!(protected_bytes(&fixture.storage), kept);
        assert!(
            asides(&fixture.storage).is_empty(),
            "the aside folder stayed"
        );
        // The run lock is the only new file.
        assert!(fixture
            .storage
            .join("retention/prune-legacy.lock")
            .is_file());
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("Removed"))
                .count(),
            legacy_folders(&fixture.storage).len()
        );
        // Harness folders keep everything except their call graph store.
        assert!(fixture.storage.join("opencode/inspect").is_dir());
        assert!(fixture.storage.join("unknown-folder/views/keep").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn prune_legacy_refuses_a_symlinked_store_and_never_follows_it() {
        let fixture = fixture();
        let outside = fixture.base.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious"), "keep me").unwrap();
        fs::remove_dir_all(fixture.storage.join("views")).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.storage.join("views")).unwrap();
        fs::remove_dir_all(fixture.storage.join("opencode/callgraph")).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.storage.join("opencode/callgraph")).unwrap();
        for yes in [false, true] {
            let report = run(&fixture.storage, yes);
            let skipped = report
                .skipped
                .iter()
                .map(|skip| (skip.path.clone(), skip.reason.clone()))
                .collect::<Vec<_>>();
            assert_eq!(skipped.len(), 2, "{skipped:?}");
            for (path, reason) in &skipped {
                assert!(
                    path.ends_with("views") || path.ends_with("opencode/callgraph"),
                    "{skipped:?}"
                );
                assert!(reason.contains("symbolic link"), "{reason}");
            }
            assert!(!found_paths(&report.stores).contains(&fixture.storage.join("views")));
        }
        assert_eq!(
            fs::read_to_string(outside.join("precious")).unwrap(),
            "keep me"
        );
        assert!(fs::symlink_metadata(fixture.storage.join("views"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn prune_legacy_refuses_a_store_on_another_device() {
        use crate::bash_background::persistence::REPAIR_IDENTITIES;
        let fixture = fixture();
        let blobs = fixture.storage.join("blobs");
        let (device, inode) = PinnedDir::open(&blobs)
            .unwrap()
            .directory_identity()
            .unwrap();
        REPAIR_IDENTITIES.with(|ids| {
            ids.borrow_mut()
                .insert(blobs.clone(), (device.wrapping_add(1), inode));
        });
        let dry = run(&fixture.storage, false);
        let report = run(&fixture.storage, true);
        REPAIR_IDENTITIES.with(|ids| ids.borrow_mut().clear());
        for report in [&dry, &report] {
            assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
            assert_eq!(report.skipped[0].path, blobs);
            assert!(
                report.skipped[0].reason.contains("another disk"),
                "{:?}",
                report.skipped
            );
        }
        assert_eq!(
            fs::read_to_string(blobs.join("ab/cdef")).unwrap(),
            "contents of blobs/ab/cdef"
        );
        assert!(!fixture.storage.join("views").exists());
    }

    #[test]
    fn prune_legacy_skips_and_reports_a_store_that_cannot_be_moved_aside() {
        let fixture = fixture();
        let kept = fs::read(fixture.storage.join("index/key-1/cache.bin")).unwrap();
        RENAME_REFUSED.with(|names| names.borrow_mut().push(OsString::from("index")));
        let mut skipped_events = 0;
        let report = prune_legacy_stores(&[], &fixture.storage, true, &mut |event| {
            if matches!(event, PruneLegacyEvent::Skipped(_)) {
                skipped_events += 1;
            }
        });
        RENAME_REFUSED.with(|names| names.borrow_mut().clear());
        let report = report.unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(skipped_events, 1);
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
        assert_eq!(report.skipped[0].path, fixture.storage.join("index"));
        assert!(
            report.skipped[0]
                .reason
                .contains("could not be moved aside"),
            "{}",
            report.skipped[0].reason
        );
        assert_eq!(
            fs::read(fixture.storage.join("index/key-1/cache.bin")).unwrap(),
            kept
        );
        assert_eq!(
            report.removed.len(),
            legacy_folders(&fixture.storage).len() - 1
        );
        assert!(!fixture.storage.join("views").exists());
        assert!(asides(&fixture.storage).is_empty());
    }

    #[test]
    fn prune_legacy_rerun_removes_what_an_interrupted_run_moved_aside() {
        let fixture = fixture();
        let kept = protected_bytes(&fixture.storage);
        STOP_AFTER_MOVE.with(|stop| stop.set(true));
        let interrupted = prune_legacy_stores(&[], &fixture.storage, true, &mut |_| {});
        STOP_AFTER_MOVE.with(|stop| stop.set(false));
        interrupted.unwrap_or_else(|error| panic!("{error:?}"));
        let left = asides(&fixture.storage);
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(left[0].join("0-artifact-owners/owner.json").is_file());
        for path in legacy_folders(&fixture.storage) {
            assert!(fs::symlink_metadata(&path).is_err(), "{}", path.display());
        }

        let dry = run(&fixture.storage, false);
        assert_eq!(found_paths(&dry.stores), left);
        assert!(dry.stores[0].leftover);
        assert!(dry.stores[0].bytes > 0);

        let report = run(&fixture.storage, true);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(found_paths(&report.removed), left);
        assert!(asides(&fixture.storage).is_empty());
        assert_eq!(protected_bytes(&fixture.storage), kept);
    }

    #[test]
    fn prune_legacy_refuses_a_folder_that_is_not_aft_storage() {
        let fixture = fixture();
        fs::remove_file(fixture.storage.join("aft.db")).unwrap();
        let before = tree(&fixture.base);
        for yes in [false, true] {
            match prune_legacy_stores(&[], &fixture.storage, yes, &mut |_| {}) {
                Err(PruneLegacyError::Refused(reason)) => {
                    assert!(reason.contains("does not look like"), "{reason}")
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert_eq!(tree(&fixture.base), before);
    }

    #[test]
    fn prune_legacy_reports_a_missing_storage_folder_as_nothing_to_do() {
        let temp = tempfile::tempdir().unwrap();
        let report = run(&temp.path().join("absent"), true);
        assert!(report.roots.is_empty());
        assert!(!temp.path().join("absent").exists());
    }

    #[test]
    fn prune_legacy_refuses_while_another_run_holds_the_lock() {
        let fixture = fixture();
        let retention = fixture.storage.join("retention");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(retention.join("prune-legacy.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        match prune_legacy_stores(&[], &fixture.storage, true, &mut |_| {}) {
            Err(PruneLegacyError::Refused(reason)) => {
                assert!(reason.contains("Another cleanup"), "{reason}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(fixture
            .storage
            .join("views/view-1/derived.sqlite")
            .is_file());
    }

    #[test]
    fn prune_legacy_also_cleans_the_enrolled_external_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        // Plain spelling on Windows, as the command reports it.
        #[cfg(windows)]
        let base = crate::windows_path::normalize_windows_path(&base);
        let storage = base.join("storage");
        let parent = base.join("cache-volume");
        fs::create_dir(&parent).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(parent.clone()),
            ..Default::default()
        };
        let warning = prepare_cache_root_for_test(&mut config);
        assert!(warning.is_none(), "{warning:?}");
        drop(config);
        fs::write(storage.join("aft.db"), "db").unwrap();
        let cache = parent.join("aft");
        for (relative, contents) in [
            ("views/a", "old"),
            ("index/key-1/cache.bin", "old"),
            ("semantic/key-1/semantic.bin", "old"),
            ("semantic/models/model.onnx", "model"),
            ("data/keep", "new"),
        ] {
            let path = cache.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let tiers = [crate::config_resolve::ConfigTier {
            tier: "user".into(),
            source: "test".into(),
            doc: serde_json::json!({"storage": {"cache_dir": parent}}).to_string(),
        }];
        let dry = run_with_tiers(&tiers, &storage, false);
        assert_eq!(dry.roots, vec![storage.clone(), cache.clone()]);
        // Only the cache root's part: preparing the cache root above may
        // leave its own folders under the storage root.
        let in_cache = |stores: &[LegacyStore]| {
            found_paths(stores)
                .into_iter()
                .filter(|path| path.starts_with(&cache))
                .collect::<Vec<_>>()
        };
        let expected = ["index", "semantic/key-1", "views"].map(|name| cache.join(name));
        assert_eq!(in_cache(&dry.stores), expected);
        let report = run_with_tiers(&tiers, &storage, true);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(in_cache(&report.removed), expected);
        for path in expected {
            assert!(!path.exists(), "{}", path.display());
        }
        assert_eq!(fs::read_to_string(cache.join("data/keep")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(cache.join("semantic/models/model.onnx")).unwrap(),
            "model"
        );
        assert!(cache.join(OWNER).is_file());
        assert!(asides(&cache).is_empty());
    }

    #[test]
    fn prune_legacy_skips_an_external_cache_root_it_cannot_prove() {
        let fixture = fixture();
        let parent = fixture.base.join("cache-volume");
        fs::create_dir_all(parent.join("aft/views")).unwrap();
        fs::write(parent.join("aft/views/a"), "someone else's").unwrap();
        let tiers = [crate::config_resolve::ConfigTier {
            tier: "user".into(),
            source: "test".into(),
            doc: serde_json::json!({"storage": {"cache_dir": parent}}).to_string(),
        }];
        let report = run_with_tiers(&tiers, &fixture.storage, true);
        assert_eq!(report.roots, vec![fixture.storage.clone()]);
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
        assert_eq!(report.skipped[0].path, parent.join("aft"));
        assert!(
            report.skipped[0].reason.contains("not proven"),
            "{}",
            report.skipped[0].reason
        );
        assert!(parent.join("aft/views/a").is_file());
    }
}

pub const CACHE_DOMAINS: [&str; 7] = [
    "views",
    "blobs",
    "callgraph",
    "semantic",
    "index",
    "symbols",
    "inspect",
];

pub(crate) fn canonical_root(project_root: &Path) -> PathBuf {
    // Nearest-existing-ancestor canonicalization, not a raw-spelling fallback:
    // registration can precede the directory's creation (and lookups can follow
    // it), and on macOS the raw tempdir spelling (/var/...) differs from the
    // canonical one (/private/var/...). Keying the access registry by whichever
    // spelling happened to win the existence race made a registered root miss
    // its own entry and fail closed.
    canonical_process_lease_dir(project_root)
}

pub(crate) fn canonical_process_lease_dir(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let normalized = lexical_normalize(path);
    let mut missing_components = Vec::new();
    let mut current = normalized.as_path();
    while !current.exists() {
        let Some(name) = current.file_name() else {
            return normalized;
        };
        missing_components.push(name.to_os_string());
        let Some(parent) = current.parent() else {
            return normalized;
        };
        current = parent;
    }

    let mut canonical = std::fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
    for component in missing_components.iter().rev() {
        canonical.push(component);
    }
    canonical
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        let Some(parent) = current.parent() else {
            return PathBuf::from(".");
        };
        current = parent;
    }
}
