//! These files coordinate safe access to a project-root cache: a writer lease
//! ensures only one process updates the cache at a time, and read-marker files
//! let cleanup see which readers are still using the cache.
//!
//! Writer leases are stored at `<storage>/callgraph/<artifact_cache_key>/writer.lease`
//! and `<storage>/inspect/<project_scope_key>/writer.lease`. They use the
//! `fs_lock` JSON format with a `writer_epoch` nonce so a writer can detect if
//! another process has taken over before publishing changes or starting SQLite
//! write transactions.
//!
//! Read markers track active SQLite readers so cache cleanup can tell when it is
//! safe to remove old data. They are stored under
//! `<cache-domain>/readers/<generation-label>/<pid>.<hostname>.<created_at_ms>.<seq>.json`;
//! the JSON records the process identity and creation time, mtime is used as a
//! heartbeat for cleanup across hosts, and the PID is used for cleanup on the
//! same host. Marker files are created `0600` so they do not expose checkout
//! activity or let another local user delete a protected marker.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
#[cfg(any(test, target_os = "linux"))]
use std::fs;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fs_lock;

#[cfg(test)]
#[path = "root_cache_paths_tests.rs"]
mod cache_paths_tests;
#[cfg(test)]
pub(crate) use location::prepare_cache_root_for_test;

pub const CACHE_DOMAINS: [&str; 7] = [
    "views",
    "blobs",
    "callgraph",
    "semantic",
    "index",
    "symbols",
    "inspect",
];

#[path = "cache_location.rs"]
mod location;
#[cfg(test)]
pub(crate) use location::acquire_cache_cleanup_lease;
pub use location::{
    acquire_daemon_storage_lease, cache_root, cache_unavailable, cache_unavailable_for_root,
    clean_old_caches, prepare_cache_root, reclaim_configured_inactive_caches, CacheLease,
    CleanupCandidate, CleanupReport, ProcessStorageLease, StoragePaths, CACHE_DIR_BUSY,
    CACHE_DIR_BUSY_MESSAGE, CACHE_DIR_UNAVAILABLE,
};
pub(crate) use location::{
    acquire_speech_serving_lease, release_quiescent_speech_serving_lease, SpeechServingLease,
};
pub(crate) use location::{
    cache_deletion_protections, cache_status_code, check_cache_access, durable_route,
    ensure_config_storage_lease, identity_storage_root, pin_cache_path, pin_owned_cache_path,
    prepare_process_cache_root, prepare_third_party_cache_dir, refuse_durable_cache_write,
    refuse_new_durable_cache_entry, register_durable_owner, retention_protections_with_budget,
    unavailable_code, unavailable_warning,
};

static MARKER_SEQ: AtomicU64 = AtomicU64::new(0);
struct LiveScope {
    count: usize,
    _marker: Option<ReadMarker>,
}
static LIVE_SCOPE_KEYS: OnceLock<Mutex<HashMap<(PathBuf, String), LiveScope>>> = OnceLock::new();

/// Read-marker heartbeats refresh no more often than the filesystem lock
/// heartbeat. Active readers piggyback this on normal read paths instead of
/// spawning a thread per connection.
pub const READ_MARKER_TOUCH_INTERVAL_MS: u64 = fs_lock::HEARTBEAT_INTERVAL_MS;
/// Cross-host markers cannot use local PID liveness, so they expire after the
/// same conservative 5x stale-heartbeat window used by filesystem locks.
pub const READ_MARKER_CROSS_HOST_STALE_MS: u64 = fs_lock::STALE_HEARTBEAT_MS * 5;
// Process start timestamps are not always millisecond-precise across OS APIs.
// A one-second grace keeps a marker created immediately after process launch
// attached to that process while still identifying clear PID reuse.
const PROCESS_START_TIME_GRACE_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RootCacheDomain {
    Callgraph,
    Inspect,
    /// Search-index publication for an unbound standing root. This remains a
    /// root-keyed WriterLease domain, separate from the session-owned cache.
    Index,
}

impl RootCacheDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            RootCacheDomain::Callgraph => "callgraph",
            RootCacheDomain::Inspect => "inspect",
            RootCacheDomain::Index => "index",
        }
    }
}

/// Serializes artifact supersession with the final disk publication step.
/// Advancing an epoch either happens before a stale worker checks and prevents
/// its publish, or after that worker has fully published the still-current
/// generation. This closes the check-then-publish race of a bare atomic epoch.
#[derive(Clone, Default)]
pub struct ArtifactPublishEpoch {
    current: Arc<parking_lot::Mutex<u64>>,
}

impl ArtifactPublishEpoch {
    pub fn next(&self) -> u64 {
        let mut current = self.current.lock();
        *current = current.wrapping_add(1);
        *current
    }

    pub fn current(&self) -> u64 {
        *self.current.lock()
    }

    pub fn is_current(&self, expected: u64) -> bool {
        *self.current.lock() == expected
    }

    pub fn run_if_current<R>(&self, expected: u64, publish: impl FnOnce() -> R) -> Option<R> {
        let current = self.current.lock();
        if *current != expected {
            return None;
        }
        Some(publish())
    }
}

pub struct WriterLease {
    domain: RootCacheDomain,
    key: String,
    path: PathBuf,
    epoch: String,
    guard: Mutex<fs_lock::LockGuard>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProcessLeaseKey {
    domain: RootCacheDomain,
    cache_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WriterLeaseAcquisitionKey {
    domain: RootCacheDomain,
    key: String,
    project_root: PathBuf,
}

static PROCESS_LEASES: OnceLock<Mutex<HashMap<ProcessLeaseKey, Weak<WriterLease>>>> =
    OnceLock::new();
// Same-root callers share this short-lived gate so only one thread performs the
// filesystem lease attempt, while different roots do not wait on the registry
// mutex during stat/probe/create/heartbeat work.
static PROCESS_LEASE_ACQUISITIONS: OnceLock<Mutex<HashMap<ProcessLeaseKey, Weak<Mutex<()>>>>> =
    OnceLock::new();
static WRITER_LEASE_ACQUISITION_COUNTS: OnceLock<Mutex<HashMap<WriterLeaseAcquisitionKey, usize>>> =
    OnceLock::new();
static WRITER_LEASE_ACQUISITION_COUNTER_ENABLED: AtomicBool = AtomicBool::new(false);
static CONFIGURED_ARTIFACT_ACCESS: OnceLock<Mutex<HashMap<PathBuf, ArtifactAccess>>> =
    OnceLock::new();
static WARNED_BORROW_ONLY_WRITES: OnceLock<Mutex<HashSet<(PathBuf, PathBuf)>>> = OnceLock::new();

/// Root-scoped capability that distinguishes shared repository artifacts from
/// mutable state private to one checkout.
#[derive(Clone, Debug)]
pub struct ArtifactAccess {
    project_root: PathBuf,
    shared_key: String,
    private_key: String,
    borrow_only_shared: bool,
    registered: bool,
}

impl ArtifactAccess {
    fn configured(project_root: &Path, shared_key: &str, borrow_only_shared: bool) -> Self {
        let project_root = canonical_root(project_root);
        Self {
            private_key: crate::path_identity::project_scope_key(&project_root),
            project_root,
            shared_key: shared_key.to_string(),
            borrow_only_shared,
            registered: true,
        }
    }

    /// Resolve the capability registered during configure. Unregistered roots
    /// receive a borrow-only capability without probing Git on the store path.
    pub fn for_root(project_root: &Path) -> Self {
        let project_root = canonical_root(project_root);
        if let Some(access) = configured_artifact_access()
            .lock()
            .ok()
            .and_then(|access| access.get(&project_root).cloned())
        {
            return access;
        }
        // Unregistered root: fail closed without spawning git probes. Configure
        // registers every bound root before any store acquisition, so landing
        // here means a direct artifact-API caller on an unconfigured root —
        // treating it as borrow-only keeps shared artifacts safe and keeps this
        // path subprocess-free (a git probe here has unbounded latency and can
        // run on latency-critical threads).
        crate::slog_warn!(
            "artifact access requested for unconfigured root {}; defaulting to borrow-only",
            project_root.display()
        );
        let shared_key = crate::path_identity::project_scope_key(&project_root);
        Self {
            private_key: shared_key.clone(),
            project_root,
            shared_key,
            borrow_only_shared: true,
            registered: false,
        }
    }

    /// Return whether this root may write the keyed artifact, logging the first
    /// denial for each concrete path so read-only degradation stays observable.
    pub fn allows_write(&self, artifact_key: &str, write_path: &Path) -> bool {
        if check_cache_access(write_path).is_err() {
            return false;
        }
        if let Err(error) = crate::production_storage::refuse_write(write_path) {
            crate::slog_warn!("{error}");
            return false;
        }
        let writes_keyed_dir = write_path
            .ancestors()
            .any(|ancestor| ancestor.file_name() == Some(OsStr::new(artifact_key)));
        if !self.borrow_only_shared
            || artifact_key != self.shared_key
            || artifact_key == self.private_key
            || !writes_keyed_dir
        {
            return true;
        }
        self.warn_write_denied_once(write_path);
        false
    }

    fn allows_writer_lease(
        &self,
        domain: RootCacheDomain,
        artifact_key: &str,
        write_path: &Path,
    ) -> bool {
        self.allows_writer_lease_inner(domain, artifact_key, write_path)
    }

    fn allows_writer_lease_inner(
        &self,
        domain: RootCacheDomain,
        artifact_key: &str,
        write_path: &Path,
    ) -> bool {
        if let Err(error) = crate::production_storage::refuse_write(write_path) {
            crate::slog_warn!("{error}");
            return false;
        }
        // Classify the layout from a comparison-local canonical copy rather
        // than trusting key equality. A key derived while the root's final
        // component was absent can encode a symlink alias (for example,
        // /var versus /private/var) that disappears after the root exists.
        let comparison_cache_dir = write_path.parent().map(canonical_process_lease_dir);
        let writes_keyed_dir = comparison_cache_dir.as_deref().and_then(Path::file_name)
            == Some(OsStr::new(artifact_key));
        let writes_root_keyed_dir = comparison_cache_dir
            .as_deref()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            == Some(OsStr::new(domain.as_str()));
        if !self.borrow_only_shared {
            return true;
        }
        if domain != RootCacheDomain::Callgraph
            && writes_keyed_dir
            && artifact_key == self.private_key
        {
            return true;
        }
        if !self.registered && !writes_root_keyed_dir {
            // Caller-owned scratch paths are outside the shared root-keyed
            // cache layout, so this capability does not govern them.
            return true;
        }
        self.warn_write_denied_once(write_path);
        false
    }

    fn warn_write_denied_once(&self, write_path: &Path) {
        let warning_key = (self.project_root.clone(), write_path.to_path_buf());
        let should_warn = WARNED_BORROW_ONLY_WRITES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .map(|mut warned| {
                if warned.len() >= 4_096 {
                    warned.clear();
                }
                warned.insert(warning_key)
            })
            .unwrap_or(false);
        if should_warn {
            crate::slog_warn!(
                "borrow-only root denied shared artifact write at {} (configured shared key {})",
                write_path.display(),
                self.shared_key
            );
        }
    }
}

fn configured_artifact_access() -> &'static Mutex<HashMap<PathBuf, ArtifactAccess>> {
    CONFIGURED_ARTIFACT_ACCESS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Return the artifact key registered during configure without deriving a new
/// key from Git or reading the older identity pin in aft.db.
pub(crate) fn configured_artifact_key(project_root: &Path) -> Option<String> {
    let project_root = canonical_root(project_root);
    configured_artifact_access()
        .lock()
        .ok()?
        .get(&project_root)
        .filter(|access| !access.shared_key.is_empty())
        .map(|access| access.shared_key.clone())
}

/// Track scope keys belonging to roots currently bound in this process. The
/// inspect sweep snapshots this registry at publication time so a live actor's
/// cache cannot be mistaken for a reclaimed worktree cache.
fn live_scope_keys() -> &'static Mutex<HashMap<(PathBuf, String), LiveScope>> {
    LIVE_SCOPE_KEYS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register_live_scope(storage_root: &Path, project_root: &Path) {
    let storage_root = canonical_root(storage_root);
    let scope_key = crate::path_identity::project_scope_key(project_root);
    let mut scopes = match live_scope_keys().lock() {
        Ok(scopes) => scopes,
        Err(poisoned) => poisoned.into_inner(),
    };
    let marker_root = storage_root.join("retention");
    let entry = scopes
        .entry((storage_root, scope_key.clone()))
        .or_insert_with(|| LiveScope {
            count: 0,
            _marker: ReadMarker::create(&marker_root, &scope_key)
                .map_err(|error| crate::slog_warn!("root residency marker unavailable: {error}"))
                .ok(),
        });
    entry.count += 1;
}

pub(crate) fn unregister_live_scope(storage_root: &Path, project_root: &Path) {
    let storage_root = canonical_root(storage_root);
    let scope_key = crate::path_identity::project_scope_key(project_root);
    let mut scopes = match live_scope_keys().lock() {
        Ok(scopes) => scopes,
        Err(poisoned) => poisoned.into_inner(),
    };
    let key = (storage_root, scope_key);
    if let Some(scope) = scopes.get_mut(&key) {
        scope.count = scope.count.saturating_sub(1);
        if scope.count == 0 {
            scopes.remove(&key);
        }
    }
}

pub(crate) fn live_scope_keys_for_storage(storage_root: &Path) -> HashSet<String> {
    let storage_root = canonical_root(storage_root);
    let scopes = match live_scope_keys().lock() {
        Ok(scopes) => scopes,
        Err(poisoned) => poisoned.into_inner(),
    };
    scopes
        .iter()
        .filter(|((root, _), scope)| root == &storage_root && scope.count > 0)
        .map(|((_, key), _)| key.clone())
        .collect()
}

/// Register the worktree topology already detected by configure so artifact
/// APIs can enforce it without repeating a Git subprocess on every write path.
pub fn configure_artifact_access(project_root: &Path, shared_key: &str, borrow_only_shared: bool) {
    let access = ArtifactAccess::configured(project_root, shared_key, borrow_only_shared);
    if let Ok(mut configured) = configured_artifact_access().lock() {
        // Bounded, but never a wholesale clear: dropping still-live roots'
        // capabilities would silently flip them onto the fail-closed
        // (borrow-only) fallback. Evict an arbitrary other entry instead —
        // any evicted-but-live root re-registers on its next configure.
        if configured.len() >= 4_096 && !configured.contains_key(&access.project_root) {
            if let Some(evict) = configured.keys().next().cloned() {
                configured.remove(&evict);
            }
        }
        configured.insert(access.project_root.clone(), access);
    }
}

pub(crate) fn canonical_root(project_root: &Path) -> PathBuf {
    // Nearest-existing-ancestor canonicalization, not a raw-spelling fallback:
    // registration can precede the directory's creation (and lookups can follow
    // it), and on macOS the raw tempdir spelling (/var/...) differs from the
    // canonical one (/private/var/...). Keying the access registry by whichever
    // spelling happened to win the existence race made a registered root miss
    // its own entry and fail closed.
    canonical_process_lease_dir(project_root)
}

fn process_leases() -> &'static Mutex<HashMap<ProcessLeaseKey, Weak<WriterLease>>> {
    PROCESS_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn process_lease_acquisitions() -> &'static Mutex<HashMap<ProcessLeaseKey, Weak<Mutex<()>>>> {
    PROCESS_LEASE_ACQUISITIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn writer_lease_acquisition_counts() -> &'static Mutex<HashMap<WriterLeaseAcquisitionKey, usize>> {
    WRITER_LEASE_ACQUISITION_COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_writer_lease_acquisition(domain: RootCacheDomain, key: &str, project_root: &Path) {
    if !WRITER_LEASE_ACQUISITION_COUNTER_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let acquisition_key = WriterLeaseAcquisitionKey {
        domain,
        key: key.to_string(),
        project_root,
    };
    if let Ok(mut counts) = writer_lease_acquisition_counts().lock() {
        *counts.entry(acquisition_key).or_default() += 1;
    }
}

/// Turn the acquisition counter on for the test process without clearing it.
///
/// There is deliberately no reset: the counter map is keyed by
/// (domain, key, project_root) and every consuming test uses its own fresh
/// temp-dir root, so each test's key starts at zero by construction. A global
/// clear here was the only cross-test coupling - under parallel execution it
/// wiped entries between another test's setup and its assertion, producing
/// isolated-green/parallel-red flakes.
#[doc(hidden)]
pub fn enable_writer_lease_acquisition_counts_for_test() {
    WRITER_LEASE_ACQUISITION_COUNTER_ENABLED.store(true, Ordering::Relaxed);
}

#[doc(hidden)]
pub fn writer_lease_acquisition_count_for_test(
    domain: RootCacheDomain,
    key: &str,
    project_root: &Path,
) -> usize {
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    writer_lease_acquisition_counts()
        .lock()
        .ok()
        .and_then(|counts| {
            counts
                .get(&WriterLeaseAcquisitionKey {
                    domain,
                    key: key.to_string(),
                    project_root,
                })
                .copied()
        })
        .unwrap_or(0)
}

fn shared_process_lease(
    registry_key: &ProcessLeaseKey,
) -> Result<Option<Arc<WriterLease>>, fs_lock::AcquireError> {
    let mut leases = process_leases().lock().map_err(|_| {
        fs_lock::AcquireError::Io(io::Error::other("process lease registry poisoned"))
    })?;
    if let Some(existing) = leases.get(registry_key).and_then(Weak::upgrade) {
        if existing.verify()? {
            return Ok(Some(existing));
        }
        leases.remove(registry_key);
    }
    Ok(None)
}

pub(crate) fn local_callgraph_writer(cache_dir: &Path) -> Option<Arc<WriterLease>> {
    shared_process_lease(&ProcessLeaseKey {
        domain: RootCacheDomain::Callgraph,
        cache_dir: canonical_process_lease_dir(cache_dir),
    })
    .ok()
    .flatten()
}

fn process_lease_acquisition_lock(
    registry_key: &ProcessLeaseKey,
) -> Result<Arc<Mutex<()>>, fs_lock::AcquireError> {
    let mut acquisitions = process_lease_acquisitions().lock().map_err(|_| {
        fs_lock::AcquireError::Io(io::Error::other(
            "process lease acquisition registry poisoned",
        ))
    })?;
    if let Some(existing) = acquisitions.get(registry_key).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    if acquisitions.len() > 1024 {
        acquisitions.retain(|_, lock| lock.strong_count() > 0);
    }
    let lock = Arc::new(Mutex::new(()));
    acquisitions.insert(registry_key.clone(), Arc::downgrade(&lock));
    Ok(lock)
}

#[cfg(test)]
type AcquireSharedHook = Arc<dyn Fn(RootCacheDomain, &Path, &str) + Send + Sync + 'static>;

#[cfg(test)]
static ACQUIRE_SHARED_HOOK: OnceLock<Mutex<Option<AcquireSharedHook>>> = OnceLock::new();

#[cfg(test)]
fn set_acquire_shared_hook_for_test(hook: Option<AcquireSharedHook>) {
    *ACQUIRE_SHARED_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("acquire shared hook mutex") = hook;
}

#[cfg(test)]
fn run_acquire_shared_hook_for_test(domain: RootCacheDomain, cache_dir: &Path, key: &str) {
    let hook = ACQUIRE_SHARED_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("acquire shared hook mutex")
        .clone();
    if let Some(hook) = hook {
        hook(domain, cache_dir, key);
    }
}

#[cfg(not(test))]
fn run_acquire_shared_hook_for_test(_domain: RootCacheDomain, _cache_dir: &Path, _key: &str) {}

impl std::fmt::Debug for WriterLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriterLease")
            .field("domain", &self.domain)
            .field("key", &self.key)
            .field("path", &self.path)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl WriterLease {
    /// Acquire a shared writer lease.
    ///
    /// Same-process callers wait without a timeout because the current lease
    /// attempt may need filesystem work that takes several seconds. The
    /// filesystem lock acquisition itself is nonblocking.
    pub fn acquire_shared(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
        project_root: &Path,
    ) -> Result<Option<Arc<Self>>, fs_lock::AcquireError> {
        Self::acquire_shared_inner(domain, cache_dir, key, project_root, None, true)
    }

    /// Acquire a process-shared writer lease without waiting past `timeout`.
    ///
    /// The deadline covers both the same-process single-flight gate and the
    /// filesystem lease. A bounded caller must not become unbounded merely
    /// because another thread is currently performing the filesystem attempt.
    pub fn acquire_shared_with_timeout(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
        project_root: &Path,
        timeout: Duration,
    ) -> Result<Option<Arc<Self>>, fs_lock::AcquireError> {
        Self::acquire_shared_inner(
            domain,
            cache_dir,
            key,
            project_root,
            Some(fs_lock::acquisition_now() + timeout),
            timeout.is_zero(),
        )
    }

    /// Keep preparation and both contention waits within the same retry budget.
    pub(crate) fn acquire_shared_until(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
        project_root: &Path,
        deadline: Instant,
    ) -> Result<Option<Arc<Self>>, fs_lock::AcquireError> {
        Self::acquire_shared_inner(domain, cache_dir, key, project_root, Some(deadline), false)
    }

    fn acquire_shared_inner(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
        project_root: &Path,
        deadline: Option<Instant>,
        filesystem_single_attempt: bool,
    ) -> Result<Option<Arc<Self>>, fs_lock::AcquireError> {
        let access = ArtifactAccess::for_root(project_root);
        if !access.allows_writer_lease(domain, key, &writer_lease_path(cache_dir)) {
            return Ok(None);
        }
        let registry_key = ProcessLeaseKey {
            domain,
            cache_dir: canonical_process_lease_dir(cache_dir),
        };
        if let Some(existing) = shared_process_lease(&registry_key)? {
            record_writer_lease_acquisition(domain, key, project_root);
            return Ok(Some(existing));
        }

        let acquisition_lock = process_lease_acquisition_lock(&registry_key)?;
        let _acquisition_guard = if let Some(deadline) = deadline {
            loop {
                match acquisition_lock.try_lock() {
                    Ok(guard) => break guard,
                    Err(TryLockError::Poisoned(_)) => {
                        return Err(fs_lock::AcquireError::Io(io::Error::other(
                            "process lease acquisition poisoned",
                        )));
                    }
                    Err(TryLockError::WouldBlock) => {
                        let now = fs_lock::acquisition_now();
                        if now >= deadline {
                            return Err(fs_lock::AcquireError::Timeout);
                        }
                        fs_lock::sleep_until_retry(Some(deadline), 10)?;
                    }
                }
            }
        } else {
            acquisition_lock.lock().map_err(|_| {
                fs_lock::AcquireError::Io(io::Error::other("process lease acquisition poisoned"))
            })?
        };

        if let Some(existing) = shared_process_lease(&registry_key)? {
            record_writer_lease_acquisition(domain, key, project_root);
            return Ok(Some(existing));
        }

        run_acquire_shared_hook_for_test(domain, cache_dir, key);

        let lease = Arc::new(Self::acquire(
            domain,
            cache_dir,
            key,
            deadline.filter(|_| !filesystem_single_attempt),
        )?);
        process_leases()
            .lock()
            .map_err(|_| {
                fs_lock::AcquireError::Io(io::Error::other("process lease registry poisoned"))
            })?
            .insert(registry_key, Arc::downgrade(&lease));
        record_writer_lease_acquisition(domain, key, project_root);
        Ok(Some(lease))
    }

    fn acquire(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
        deadline: Option<Instant>,
    ) -> Result<Self, fs_lock::AcquireError> {
        if !storage_allows_root_keyed(cache_dir)? {
            return Err(fs_lock::AcquireError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing root-keyed {} writer lease on a network filesystem at {}",
                    domain.as_str(),
                    cache_dir.display()
                ),
            )));
        }
        if let Some(storage_root) = cache_dir.parent().and_then(Path::parent) {
            crate::legacy_partitions::guard_new_layout_write_path(
                storage_root,
                cache_dir,
                "root-keyed writer lease",
            )?;
        }
        crate::private_storage::open_keyed_dir(cache_dir, domain.as_str())?;
        let guard = match deadline {
            Some(deadline) => fs_lock::try_acquire_until(&writer_lease_path(cache_dir), deadline)?,
            None => fs_lock::try_acquire_once(&writer_lease_path(cache_dir))?,
        };
        if !guard.verify_writer_epoch()? {
            return Err(fs_lock::AcquireError::Io(io::Error::other(
                "writer lease epoch changed immediately after acquisition",
            )));
        }
        let path = guard.path().to_path_buf();
        let epoch = guard.writer_epoch().to_string();
        Ok(Self {
            domain,
            key: key.to_string(),
            path,
            epoch,
            guard: Mutex::new(guard),
        })
    }

    pub fn verify(&self) -> io::Result<bool> {
        self.guard
            .lock()
            .map_err(|_| io::Error::other("writer lease mutex poisoned"))?
            .verify_writer_epoch()
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn domain(&self) -> RootCacheDomain {
        self.domain
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn writer_lease_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("writer.lease")
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ReadMarkerMetadata {
    pub pid: u32,
    pub hostname: String,
    pub created_at_ms: u64,
    /// Per-acquisition identity of a base-reference marker. Ordinary readers
    /// leave it out, so their files are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

#[derive(Debug)]
pub struct ReadMarker {
    path: PathBuf,
    generation: crate::bash_background::persistence::PinnedDir,
    name: std::ffi::OsString,
    metadata: ReadMarkerMetadata,
    last_touched_at_ms: AtomicU64,
}

impl ReadMarker {
    pub fn create(cache_dir: &Path, generation_label: &str) -> io::Result<Self> {
        Self::create_inner(cache_dir, generation_label, None)
    }

    /// A marker whose file also records `nonce`, so its holder can later
    /// prove the file at its unique path is still its own acquisition.
    pub fn create_with_nonce(
        cache_dir: &Path,
        generation_label: &str,
        nonce: &str,
    ) -> io::Result<Self> {
        Self::create_inner(cache_dir, generation_label, Some(nonce.to_owned()))
    }

    /// True when the marker file at this marker's own path still exists and
    /// carries this marker's metadata, nonce included. A sweep that judged the
    /// owner dead removes the file, and this then reports `false`.
    pub fn verify_held(&self) -> io::Result<bool> {
        use std::io::Read;
        let mut file = match self.generation.repair_read_file(&self.name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        (&mut file).take(32 * 1024).read_to_end(&mut bytes)?;
        Ok(serde_json::from_slice::<ReadMarkerMetadata>(&bytes)
            .is_ok_and(|stored| stored == self.metadata))
    }

    fn create_inner(
        cache_dir: &Path,
        generation_label: &str,
        nonce: Option<String>,
    ) -> io::Result<Self> {
        let metadata = ReadMarkerMetadata {
            pid: std::process::id(),
            hostname: current_hostname(),
            created_at_ms: now_ms(),
            nonce,
        };
        let dir = read_marker_dir(cache_dir, generation_label);
        crate::private_storage::open_dir(cache_dir, &dir)?;
        let seq = MARKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{}.{}.{}.{}.json",
            metadata.pid,
            sanitize_marker_component(&metadata.hostname),
            metadata.created_at_ms,
            seq
        ));
        let generation = match pin_cache_path(&dir)? {
            Some(access) => access.directory(false)?,
            None => crate::bash_background::persistence::PinnedDir::repair_open_path(&dir)?,
        };
        let name = path.file_name().unwrap().to_os_string();
        write_marker_file_pinned(&generation, &name, &metadata)?;
        let last_touched_at_ms = AtomicU64::new(metadata.created_at_ms);
        Ok(Self {
            path,
            generation,
            name,
            metadata,
            last_touched_at_ms,
        })
    }

    pub fn touch(&self) -> io::Result<()> {
        write_marker_file_pinned(&self.generation, &self.name, &self.metadata)?;
        self.last_touched_at_ms.store(now_ms(), Ordering::Relaxed);
        Ok(())
    }

    pub fn touch_if_due(&self) -> io::Result<()> {
        let now = now_ms();
        let last = self.last_touched_at_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < READ_MARKER_TOUCH_INTERVAL_MS {
            return Ok(());
        }
        self.touch()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> &ReadMarkerMetadata {
        &self.metadata
    }
}

impl Drop for ReadMarker {
    fn drop(&mut self) {
        let _ = self.generation.repair_remove_file(&self.name);
    }
}

#[cfg(unix)]
#[test]
fn reader_marker_drop_cannot_unlink_through_a_retargeted_generation_ancestor() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let marker = ReadMarker::create(&cache, "generation").unwrap();
    let name = marker.path().file_name().unwrap().to_os_string();
    let outside = temp.path().join("outside");
    fs::create_dir_all(outside.join("generation")).unwrap();
    let precious = outside.join("generation").join(&name);
    fs::write(&precious, "precious").unwrap();
    let parked = temp.path().join("parked-readers");
    fs::rename(cache.join("readers"), &parked).unwrap();
    std::os::unix::fs::symlink(&outside, cache.join("readers")).unwrap();
    drop(marker);
    assert_eq!(fs::read_to_string(&precious).unwrap(), "precious");
    assert!(!parked.join("generation").join(name).exists());
}

pub fn read_marker_dir(cache_dir: &Path, generation_label: &str) -> PathBuf {
    cache_dir.join("readers").join(generation_label)
}

/// Sweep generation markers through pinned handles without following links.
#[cfg(test)]
pub(crate) fn sweep_all_read_markers(cache_dir: &Path) -> ReadMarkerSweep {
    let result = crate::bash_background::persistence::PinnedDir::open(cache_dir).and_then(|dir| {
        sweep_all_read_markers_pinned(
            &dir,
            &mut crate::bash_background::persistence::RepairBudget::new(),
            true,
        )
    });
    result.unwrap_or(ReadMarkerSweep {
        protected: true,
        removed_stale: 0,
    })
}

pub(crate) fn sweep_all_read_markers_pinned(
    cache: &crate::bash_background::persistence::PinnedDir,
    budget: &mut crate::bash_background::persistence::RepairBudget,
    remove_stale: bool,
) -> io::Result<ReadMarkerSweep> {
    let readers = match cache.repair_child(std::ffi::OsStr::new("readers")) {
        Ok(readers) => readers,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ReadMarkerSweep::default())
        }
        Err(error) => {
            return Err(io::Error::other(format!(
                "readers is a symlink or non-directory: {error}"
            )))
        }
    };
    readers.repair_check_boundary(cache, &[])?;
    let mut sweep = ReadMarkerSweep::default();
    for name in readers.repair_names(budget)? {
        let generation = readers.repair_child_same_mount(&name)?;
        let result = sweep_marker_generation(&generation, budget, remove_stale)?;
        sweep.protected |= result.protected;
        sweep.removed_stale += result.removed_stale;
    }
    Ok(sweep)
}

fn sweep_marker_generation(
    generation: &crate::bash_background::persistence::PinnedDir,
    budget: &mut crate::bash_background::persistence::RepairBudget,
    remove_stale: bool,
) -> io::Result<ReadMarkerSweep> {
    sweep_marker_generation_cached(
        generation,
        budget,
        remove_stale,
        now_ms(),
        &current_hostname(),
        &mut HashMap::new(),
    )
}

/// [`sweep_marker_generation`] with the caller-supplied clock, hostname and
/// process-start cache. The storage sweep visits many generations per pass
/// and keeps all three in its pass context instead of re-reading the hostname
/// and each live pid's start time once per generation.
fn sweep_marker_generation_cached(
    generation: &crate::bash_background::persistence::PinnedDir,
    budget: &mut crate::bash_background::persistence::RepairBudget,
    remove_stale: bool,
    now: u64,
    host: &str,
    started: &mut HashMap<u32, Option<u64>>,
) -> io::Result<ReadMarkerSweep> {
    let mut sweep = ReadMarkerSweep::default();
    let mut names = generation.repair_names(budget)?;
    names.sort();
    for name in names {
        #[cfg(test)]
        BEFORE_MARKER_OPEN.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(&name);
            }
        });
        let file = match generation.repair_read_file(&name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match marker_file_is_protected(file, now, host, started)? {
            MarkerProtection::Protected => sweep.protected = true,
            MarkerProtection::Stale | MarkerProtection::Malformed if remove_stale => {
                match generation.repair_remove_file(&name) {
                    Ok(()) => sweep.removed_stale += 1,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            _ => {}
        }
    }
    Ok(sweep)
}

#[cfg(test)]
thread_local! { static BEFORE_MARKER_OPEN: std::cell::RefCell<Option<Box<dyn FnMut(&std::ffi::OsStr)>>> = const { std::cell::RefCell::new(None) }; }

#[test]
fn reader_census_preserves_live_protection_when_another_marker_exits() {
    let temp = tempfile::tempdir().unwrap();
    let marker = ReadMarker::create(temp.path(), "generation").unwrap();
    let generation = temp.path().join("readers/generation");
    fs::copy(marker.path(), generation.join("000-live")).unwrap();
    let departing = generation.join("zzz-exiting");
    fs::write(&departing, "departing").unwrap();
    let seen_live = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = seen_live.clone();
    BEFORE_MARKER_OPEN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |name| {
            if name == "000-live" {
                seen.store(true, Ordering::SeqCst);
            }
            if name == "zzz-exiting" {
                assert!(seen.load(Ordering::SeqCst));
                fs::remove_file(&departing).unwrap();
            }
        }))
    });
    let sweep = read_marker_protection(temp.path(), "generation", false);
    BEFORE_MARKER_OPEN.with(|hook| hook.borrow_mut().take());
    assert!(seen_live.load(Ordering::SeqCst));
    assert!(
        sweep.protected,
        "a vanished marker erased a surviving reader's protection: {sweep:?}"
    );
}

#[test]
fn reader_census_does_not_treat_an_unavailable_cache_root_as_empty() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing-cache-root");
    assert!(read_marker_protection(&missing, "generation", false).protected);
}

#[cfg(unix)]
#[test]
fn marker_sweep_stays_on_pinned_generation_after_ancestor_swap() {
    use crate::bash_background::persistence::{PinnedDir, RepairBudget};
    let temp = tempfile::tempdir().unwrap();
    let readers = temp.path().join("readers");
    let generation = readers.join("generation");
    fs::create_dir_all(&generation).unwrap();
    fs::write(generation.join("marker.json"), "malformed-own").unwrap();
    let outside = temp.path().join("foreign");
    fs::create_dir_all(outside.join("generation")).unwrap();
    let precious = outside.join("generation/marker.json");
    fs::write(&precious, "precious").unwrap();
    let pinned = PinnedDir::open(&generation).unwrap();
    let parked = temp.path().join("parked");
    fs::rename(&readers, &parked).unwrap();
    std::os::unix::fs::symlink(&outside, &readers).unwrap();
    let result = sweep_marker_generation(&pinned, &mut RepairBudget::new(), true).unwrap();
    assert_eq!(result.removed_stale, 1);
    assert_eq!(fs::read_to_string(precious).unwrap(), "precious");
    assert!(!parked.join("generation/marker.json").exists());
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadMarkerSweep {
    pub protected: bool,
    pub removed_stale: usize,
}

pub fn protected_read_marker_exists(cache_dir: &Path, generation_label: &str) -> bool {
    read_marker_protection(cache_dir, generation_label, false).protected
}

pub fn sweep_read_markers(cache_dir: &Path, generation_label: &str) -> ReadMarkerSweep {
    read_marker_protection(cache_dir, generation_label, true)
}

/// A sweep may retain a positive census across its epoch bump: an extra mark
/// only postpones garbage collection. Never cache a negative result, because a
/// reader can register before the deleting mark. Removal checks must bypass
/// this cache entirely and inspect protection again under the registry barrier.
#[derive(Default)]
pub(crate) struct ReadMarkerProtectionCache {
    protected: std::collections::BTreeMap<(PathBuf, String), bool>,
}

impl ReadMarkerProtectionCache {
    pub(crate) fn protected(&mut self, cache_dir: &Path, generation: &str) -> bool {
        let key = (cache_dir.to_path_buf(), generation.to_owned());
        if let Some(protected) = self.protected.get(&key) {
            return *protected;
        }
        let protected = protected_read_marker_exists(cache_dir, generation);
        if protected {
            self.protected.insert(key, protected);
        }
        protected
    }
}

/// Marker inputs a storage sweep re-reads for every generation it visits:
/// the process hostname and each live pid's start time. The hostname cannot
/// change mid-pass and a cached start time only ever over-retains (pids are
/// reused with ever newer start times), so one pass keeps one copy of each
/// instead of re-reading them once per generation or once per marker.
/// Liveness itself is still probed per marker and never cached.
pub(crate) struct ReadMarkerSweepContext {
    hostname: String,
    started: HashMap<u32, Option<u64>>,
}

impl ReadMarkerSweepContext {
    pub(crate) fn new() -> Self {
        Self {
            hostname: current_hostname(),
            started: HashMap::new(),
        }
    }

    /// [`sweep_read_markers`] for one generation of an already-pinned
    /// `<view>/readers` directory. The caller opens the view and its readers
    /// once per view and sweeps each generation from that handle, instead of
    /// re-opening both once per generation. Vanished generations stay
    /// unprotected and unreadable ones stay protected, as with the
    /// pathname-based sweep.
    pub(crate) fn sweep_read_markers_from_readers(
        &mut self,
        readers: &crate::bash_background::persistence::PinnedDir,
        generation_label: &str,
        remove_stale: bool,
    ) -> ReadMarkerSweep {
        let Self { hostname, started } = self;
        let result = (|| {
            let generation =
                match readers.repair_child_same_mount(std::ffi::OsStr::new(generation_label)) {
                    Ok(generation) => generation,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok(ReadMarkerSweep::default())
                    }
                    Err(error) => return Err(error),
                };
            sweep_marker_generation_cached(
                &generation,
                &mut crate::bash_background::persistence::RepairBudget::new(),
                remove_stale,
                now_ms(),
                hostname,
                started,
            )
        })();
        match result {
            Ok(sweep) => sweep,
            Err(_) => ReadMarkerSweep {
                protected: true,
                removed_stale: 0,
            },
        }
    }
}

// Test hook counting hostname resolutions on this thread. Reset before a
// marker pass and read after it to prove the pass resolved the hostname
// once instead of once per generation.
#[cfg(test)]
thread_local! {
    static HOSTNAME_RESOLUTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn hostname_resolutions_for_test() -> u64 {
    HOSTNAME_RESOLUTIONS.with(|count| count.get())
}

#[cfg(test)]
pub(crate) fn reset_hostname_resolutions_for_test() {
    HOSTNAME_RESOLUTIONS.with(|count| count.set(0));
}

// Test hook counting process-start-time resolutions on this thread: cache
// misses while attributing markers, not liveness probes (which are never
// cached). Reset before a marker pass and read after it to prove repeated
// markers from one process resolved its start time once.
#[cfg(test)]
thread_local! {
    static PROCESS_START_RESOLUTIONS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn process_start_resolutions_for_test() -> u64 {
    PROCESS_START_RESOLUTIONS.with(|count| count.get())
}

#[cfg(test)]
pub(crate) fn reset_process_start_resolutions_for_test() {
    PROCESS_START_RESOLUTIONS.with(|count| count.set(0));
}

fn read_marker_protection(
    cache_dir: &Path,
    generation_label: &str,
    remove_stale: bool,
) -> ReadMarkerSweep {
    #[cfg(test)]
    READER_CENSUS.with(|reads| {
        if let Some(reads) = reads.borrow_mut().as_mut() {
            *reads
                .entry((cache_dir.to_path_buf(), generation_label.to_owned()))
                .or_default() += 1;
        }
    });
    let result = (|| {
        let cache = crate::bash_background::persistence::PinnedDir::open(cache_dir)?;
        let readers = match cache.repair_child_same_mount(std::ffi::OsStr::new("readers")) {
            Ok(readers) => readers,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ReadMarkerSweep::default())
            }
            Err(error) => return Err(error),
        };
        let generation =
            match readers.repair_child_same_mount(std::ffi::OsStr::new(generation_label)) {
                Ok(generation) => generation,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(ReadMarkerSweep::default())
                }
                Err(error) => return Err(error),
            };
        sweep_marker_generation(
            &generation,
            &mut crate::bash_background::persistence::RepairBudget::new(),
            remove_stale,
        )
    })();
    match result {
        Ok(sweep) => sweep,
        Err(_) => ReadMarkerSweep {
            protected: true,
            removed_stale: 0,
        },
    }
}

#[cfg(test)]
thread_local! {
    static READER_CENSUS: std::cell::RefCell<Option<std::collections::BTreeMap<(PathBuf, String), usize>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct ReaderCensusProbe;

#[cfg(test)]
impl ReaderCensusProbe {
    pub(crate) fn start() -> Self {
        READER_CENSUS.with(|reads| *reads.borrow_mut() = Some(Default::default()));
        Self
    }

    pub(crate) fn finish(self) -> std::collections::BTreeMap<(PathBuf, String), usize> {
        READER_CENSUS.with(|reads| reads.borrow_mut().take().unwrap())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerProtection {
    Protected,
    Stale,
    Malformed,
}

fn marker_file_is_protected(
    file: File,
    now: u64,
    current_host: &str,
    started: &mut HashMap<u32, Option<u64>>,
) -> io::Result<MarkerProtection> {
    use std::io::Read;
    let file_metadata = file.metadata()?;
    // Marker records are tiny. Refuse oversized input rather than allocating
    // an unbounded buffer or interpreting a partial record as stale.
    if file_metadata.len() > 32 * 1024 {
        return Err(io::Error::other("read marker exceeds byte limit"));
    }
    let mut bytes = Vec::new();
    file.take(32 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 32 * 1024 {
        return Err(io::Error::other("read marker exceeds byte limit"));
    }
    let metadata: ReadMarkerMetadata = match serde_json::from_slice(&bytes) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(MarkerProtection::Malformed),
    };
    if metadata.hostname != current_host {
        let mtime_ms = file_metadata
            .modified()
            .ok()
            .map(system_time_ms)
            .unwrap_or(now);
        return Ok(
            if now.saturating_sub(mtime_ms) <= READ_MARKER_CROSS_HOST_STALE_MS {
                MarkerProtection::Protected
            } else {
                MarkerProtection::Stale
            },
        );
    }
    Ok(
        if fs_lock::process_alive(metadata.pid) && marker_start_matches(&metadata, started) {
            MarkerProtection::Protected
        } else {
            MarkerProtection::Stale
        },
    )
}

fn marker_start_matches(
    metadata: &ReadMarkerMetadata,
    started: &mut HashMap<u32, Option<u64>>,
) -> bool {
    // Same-host PID liveness is authoritative for the process instance. When the
    // OS can tell us the live PID started after this marker was created, the PID
    // has been reused and the marker belongs to a dead prior process; otherwise
    // a live PID protects the marker without consulting marker mtime.
    let started_at_ms = match started.get(&metadata.pid) {
        Some(cached) => *cached,
        None => {
            #[cfg(test)]
            PROCESS_START_RESOLUTIONS.with(|count| count.set(count.get() + 1));
            let resolved = process_start_time_ms(metadata.pid);
            started.insert(metadata.pid, resolved);
            resolved
        }
    };
    // A cached start time only ever over-retains: PIDs are reused with ever
    // newer start times, so a cached (older) start protects a superset of the
    // markers the current start would protect. A negative answer is never
    // cached: liveness itself is re-probed per marker above.
    started_at_ms
        .map(|started_at_ms| {
            started_at_ms
                <= metadata
                    .created_at_ms
                    .saturating_add(PROCESS_START_TIME_GRACE_MS)
        })
        .unwrap_or(true)
}

#[cfg(test)]
fn write_marker_file(path: &Path, metadata: &ReadMarkerMetadata) -> io::Result<()> {
    let parent = crate::bash_background::persistence::PinnedDir::repair_open_path(
        path.parent()
            .ok_or_else(|| io::Error::other("marker has no parent"))?,
    )?;
    write_marker_file_pinned(&parent, path.file_name().unwrap(), metadata)
}

fn write_marker_file_pinned(
    generation: &crate::bash_background::persistence::PinnedDir,
    name: &std::ffi::OsStr,
    metadata: &ReadMarkerMetadata,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(metadata).map_err(io::Error::other)?;
    bytes.push(b'\n');
    generation.repair_replace_file(name, &bytes)
}

fn sanitize_marker_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
static FORCE_NETWORK_FS_FOR_TEST: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub fn set_force_network_fs_for_test(enabled: bool) {
    FORCE_NETWORK_FS_FOR_TEST.store(enabled, Ordering::SeqCst);
}

pub fn storage_allows_root_keyed(path: &Path) -> io::Result<bool> {
    #[cfg(test)]
    if FORCE_NETWORK_FS_FOR_TEST.load(Ordering::SeqCst) {
        return Ok(false);
    }

    let probe = existing_ancestor(path);
    filesystem_is_local(&probe)
}

fn canonical_process_lease_dir(path: &Path) -> PathBuf {
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

fn existing_ancestor(path: &Path) -> PathBuf {
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

#[cfg(target_os = "macos")]
fn filesystem_is_local(path: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let nul = stat
        .f_fstypename
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(stat.f_fstypename.len());
    let fs_type = String::from_utf8_lossy(
        &stat.f_fstypename[..nul]
            .iter()
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>(),
    )
    .to_ascii_lowercase();
    Ok(!matches!(
        fs_type.as_str(),
        "nfs" | "smbfs" | "afpfs" | "webdav" | "fusefs"
    ))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn filesystem_is_local(path: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fs_type = stat.f_type as i64;
    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const SMB_SUPER_MAGIC: i64 = 0x517B;
    const CIFS_MAGIC_NUMBER: i64 = 0xFF534D42;
    Ok(!matches!(
        fs_type,
        NFS_SUPER_MAGIC | SMB_SUPER_MAGIC | CIFS_MAGIC_NUMBER
    ))
}

#[cfg(not(unix))]
fn filesystem_is_local(_path: &Path) -> io::Result<bool> {
    Ok(true)
}

fn now_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
static PROCESS_START_TIME_OVERRIDES: OnceLock<Mutex<HashMap<u32, Option<u64>>>> = OnceLock::new();

#[cfg(test)]
fn set_process_start_time_for_test(pid: u32, started_at_ms: Option<u64>) {
    PROCESS_START_TIME_OVERRIDES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("process start override mutex")
        .insert(pid, started_at_ms);
}

#[cfg(test)]
fn clear_process_start_time_for_test(pid: u32) {
    if let Some(overrides) = PROCESS_START_TIME_OVERRIDES.get() {
        overrides
            .lock()
            .expect("process start override mutex")
            .remove(&pid);
    }
}

#[cfg(test)]
fn process_start_time_override(pid: u32) -> Option<Option<u64>> {
    PROCESS_START_TIME_OVERRIDES
        .get()
        .and_then(|overrides| overrides.lock().ok()?.get(&pid).copied())
}

#[cfg(target_os = "linux")]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields = after_comm.split_whitespace().collect::<Vec<_>>();
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    let boot_time_secs = boot_time_secs()?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let ticks_per_second = ticks_per_second as u64;
    Some(
        boot_time_secs
            .saturating_mul(1_000)
            .saturating_add(start_ticks.saturating_mul(1_000) / ticks_per_second),
    )
}

#[cfg(target_os = "linux")]
fn read_boot_time_secs() -> Option<u64> {
    fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
}

#[cfg(target_os = "linux")]
fn boot_time_secs() -> Option<u64> {
    static BOOT_TIME_SECS: OnceLock<Option<u64>> = OnceLock::new();
    cached_boot_time_secs(&BOOT_TIME_SECS, read_boot_time_secs)
}

/// Read once per process: boot time cannot change while the process runs.
/// Shared with the platform-independent test that proves the reader runs once.
#[cfg(any(target_os = "linux", test))]
fn cached_boot_time_secs(
    cache: &OnceLock<Option<u64>>,
    reader: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    *cache.get_or_init(reader)
}

#[cfg(target_os = "macos")]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    // `proc_pidinfo(PROC_PIDTBSDINFO)` answers only for the caller's own uid;
    // for another user's process it returns zero bytes with no errno, which
    // read as "no start-time source" and left a recycled low PID (a system
    // daemon after reboot) counted as the live owner of an old log. The
    // kern.proc.pid sysctl is what `ps` uses and answers for every pid. The
    // reply is a kinfo_proc whose first member is extern_proc's p_un union,
    // where p_starttime (a timeval) sits at offset 0; the libc crate does not
    // declare the struct, so the two fields are read from the raw buffer.
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    let mut buffer = [0_u8; 1024];
    let mut size = buffer.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 16 {
        return None;
    }
    let tv_sec = i64::from_ne_bytes(buffer[0..8].try_into().ok()?);
    let tv_usec = i32::from_ne_bytes(buffer[8..12].try_into().ok()?);
    if tv_sec <= 0 {
        return None;
    }
    Some(
        (tv_sec as u64)
            .saturating_mul(1_000)
            .saturating_add(tv_usec.max(0) as u64 / 1_000),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }
    #[cfg(windows)]
    {
        crate::fs_lock::windows_process_start_time_ms(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

#[cfg(unix)]
fn current_hostname() -> String {
    #[cfg(test)]
    HOSTNAME_RESOLUTIONS.with(|count| count.set(count.get() + 1));
    let mut buffer = [0u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result == 0 {
        let len = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        if len > 0 {
            return String::from_utf8_lossy(&buffer[..len]).into_owned();
        }
    }
    "unknown-host".to_string()
}

#[cfg(windows)]
fn current_hostname() -> String {
    #[cfg(test)]
    HOSTNAME_RESOLUTIONS.with(|count| count.set(count.get() + 1));
    crate::environment::non_empty_var("COMPUTERNAME").unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(all(not(unix), not(windows)))]
fn current_hostname() -> String {
    "unknown-host".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durability_read_marker_count() {
        let dir = tempfile::tempdir().unwrap();
        crate::durability::take();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        marker.touch().unwrap();
        assert!(protected_read_marker_exists(dir.path(), "current"));
        drop(marker);
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
    }

    #[test]
    fn read_marker_file_is_private_and_touchable() {
        let dir = tempfile::tempdir().unwrap();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        assert!(marker.path().is_file());
        marker.touch().unwrap();
        let bytes = fs::read(marker.path()).unwrap();
        let parsed: ReadMarkerMetadata = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.pid, std::process::id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(marker.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn same_host_live_marker_ignores_stale_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        filetime::set_file_mtime(marker.path(), filetime::FileTime::from_unix_time(0, 0)).unwrap();

        assert!(protected_read_marker_exists(dir.path(), "current"));
    }

    #[test]
    fn sweep_removes_dead_same_host_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("dead.json");
        let metadata = ReadMarkerMetadata {
            pid: 0,
            hostname: current_hostname(),
            created_at_ms: now_ms(),
            nonce: None,
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();

        let sweep = sweep_read_markers(dir.path(), "old");

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[test]
    fn sweep_removes_reused_pid_marker_when_created_at_predates_process_start() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("reused.json");
        let pid = std::process::id();
        let metadata = ReadMarkerMetadata {
            pid,
            hostname: current_hostname(),
            created_at_ms: 1_000,
            nonce: None,
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();
        set_process_start_time_for_test(pid, Some(10_000));

        let sweep = sweep_read_markers(dir.path(), "old");
        clear_process_start_time_for_test(pid);

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[test]
    fn sweep_removes_expired_cross_host_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("cross-host.json");
        let metadata = ReadMarkerMetadata {
            pid: 123,
            hostname: format!("other-{}", current_hostname()),
            created_at_ms: now_ms(),
            nonce: None,
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();
        let stale_time = SystemTime::now()
            .checked_sub(Duration::from_millis(
                READ_MARKER_CROSS_HOST_STALE_MS.saturating_add(1_000),
            ))
            .unwrap_or(UNIX_EPOCH);
        filetime::set_file_mtime(
            &marker_path,
            filetime::FileTime::from_system_time(stale_time),
        )
        .unwrap();

        let sweep = sweep_read_markers(dir.path(), "old");

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn process_lease_dir_canonicalizes_existing_ancestor_before_cache_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let missing_cache_dir = link.join("inspect").join("project");
        let before_create = canonical_process_lease_dir(&missing_cache_dir);
        fs::create_dir_all(&missing_cache_dir).unwrap();
        let after_create = canonical_process_lease_dir(&missing_cache_dir);

        assert_eq!(before_create, after_create);
        assert!(before_create.starts_with(std::fs::canonicalize(&real).unwrap()));
    }

    #[test]
    fn borrow_only_root_never_receives_existing_shared_writer_capability() {
        let storage = tempfile::tempdir().unwrap();
        let parent_root = tempfile::tempdir().unwrap();
        let worktree_root = tempfile::tempdir().unwrap();
        let shared_key = "shared-artifact-key";
        let cache_dir = storage.path().join("callgraph").join(shared_key);
        configure_artifact_access(parent_root.path(), shared_key, false);
        configure_artifact_access(worktree_root.path(), shared_key, true);

        let parent_lease = WriterLease::acquire_shared(
            RootCacheDomain::Callgraph,
            &cache_dir,
            shared_key,
            parent_root.path(),
        )
        .unwrap()
        .expect("parent writer lease");
        enable_writer_lease_acquisition_counts_for_test();

        let worktree_lease = WriterLease::acquire_shared(
            RootCacheDomain::Callgraph,
            &cache_dir,
            shared_key,
            worktree_root.path(),
        )
        .unwrap();
        let unkeyed_worktree_lease = WriterLease::acquire_shared(
            RootCacheDomain::Callgraph,
            &storage.path().join("scratch"),
            shared_key,
            worktree_root.path(),
        )
        .unwrap();

        assert!(worktree_lease.is_none());
        assert!(unkeyed_worktree_lease.is_none());
        assert!(parent_lease.verify().unwrap());
        assert_eq!(
            writer_lease_acquisition_count_for_test(
                RootCacheDomain::Callgraph,
                shared_key,
                worktree_root.path(),
            ),
            0
        );
    }

    #[test]
    fn unregistered_root_cannot_acquire_keyed_shared_writer_lease() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shared_key = "unregistered-shared-key";
        let cache_dir = storage.path().join("callgraph").join(shared_key);
        enable_writer_lease_acquisition_counts_for_test();

        let lease = WriterLease::acquire_shared(
            RootCacheDomain::Callgraph,
            &cache_dir,
            shared_key,
            root.path(),
        )
        .unwrap();

        assert!(lease.is_none());
        assert_eq!(
            writer_lease_acquisition_count_for_test(
                RootCacheDomain::Callgraph,
                shared_key,
                root.path(),
            ),
            0
        );
        assert!(!writer_lease_path(&cache_dir).exists());
    }

    #[cfg(unix)]
    #[test]
    fn unregistered_root_denies_symlink_aliased_stale_key() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let real_parent = roots.path().join("real");
        let alias_parent = roots.path().join("alias");
        fs::create_dir_all(&real_parent).unwrap();
        std::os::unix::fs::symlink(&real_parent, &alias_parent).unwrap();

        let root = alias_parent.join("root");
        let stale_key = crate::search_index::artifact_cache_key(&root);
        fs::create_dir_all(&root).unwrap();
        let canonical_key = crate::search_index::artifact_cache_key(&root);
        assert_ne!(stale_key, canonical_key, "fixture must exercise the alias");

        let cache_dir = storage.path().join("callgraph").join(&stale_key);
        enable_writer_lease_acquisition_counts_for_test();
        let lease = WriterLease::acquire_shared(
            RootCacheDomain::Callgraph,
            &cache_dir,
            &canonical_key,
            &root,
        )
        .unwrap();

        assert!(lease.is_none());
        assert_eq!(
            writer_lease_acquisition_count_for_test(
                RootCacheDomain::Callgraph,
                &canonical_key,
                &root,
            ),
            0
        );
        assert!(!writer_lease_path(&cache_dir).exists());
    }

    #[cfg(unix)]
    #[test]
    fn registration_before_directory_creation_survives_alias_canonicalization() {
        // Regression for the concurrent-acquire flake: configure registered the
        // root before its directory existed (raw alias spelling), the first
        // lease's create_dir_all made later lookups canonicalize to the real
        // spelling, and the registered root missed its own capability entry.
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let real_parent = roots.path().join("real");
        let alias_parent = roots.path().join("alias");
        fs::create_dir_all(&real_parent).unwrap();
        std::os::unix::fs::symlink(&real_parent, &alias_parent).unwrap();

        let root = alias_parent.join("pre-registered");
        configure_artifact_access(&root, "pre-registered", false);
        fs::create_dir_all(&root).unwrap();

        let cache_dir = storage.path().join("inspect").join("pre-registered");
        let lease = WriterLease::acquire_shared(
            RootCacheDomain::Inspect,
            &cache_dir,
            "pre-registered",
            &root,
        )
        .unwrap();
        assert!(
            lease.is_some(),
            "a root registered before its directory existed must keep its capability"
        );
    }

    #[test]
    fn borrow_only_root_keeps_private_project_scope_writable() {
        let storage = tempfile::tempdir().unwrap();
        let worktree_root = tempfile::tempdir().unwrap();
        let shared_key = "shared-artifact-key";
        let private_key = crate::path_identity::project_scope_key(worktree_root.path());
        let cache_dir = storage.path().join("inspect").join(&private_key);
        configure_artifact_access(worktree_root.path(), shared_key, true);

        let lease = WriterLease::acquire_shared(
            RootCacheDomain::Inspect,
            &cache_dir,
            &private_key,
            worktree_root.path(),
        )
        .unwrap()
        .expect("private inspect writer lease");

        assert!(lease.verify().unwrap());
    }

    #[test]
    fn writer_lease_acquire_shared_does_not_serialize_different_roots() {
        let dir = tempfile::tempdir().unwrap();
        let blocked_cache_dir = dir.path().join("callgraph").join("blocked");
        let free_cache_dir = dir.path().join("callgraph").join("free");
        configure_artifact_access(&blocked_cache_dir, "blocked", false);
        configure_artifact_access(&free_cache_dir, "free", false);
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));

        struct HookGuard;
        impl Drop for HookGuard {
            fn drop(&mut self) {
                set_acquire_shared_hook_for_test(None);
            }
        }

        set_acquire_shared_hook_for_test(Some(Arc::new(move |_, _, key| {
            if key == "blocked" {
                blocked_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
        })));
        let _hook_guard = HookGuard;

        let blocked_handle = std::thread::spawn(move || {
            WriterLease::acquire_shared(
                RootCacheDomain::Callgraph,
                &blocked_cache_dir,
                "blocked",
                &blocked_cache_dir,
            )
            .map_err(|error| error.to_string())
            .and_then(|lease| lease.ok_or_else(|| "writer lease unexpectedly denied".to_string()))
        });
        blocked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocked root should reach acquisition hook");

        let (free_tx, free_rx) = std::sync::mpsc::channel();
        let free_handle = std::thread::spawn(move || {
            let result = WriterLease::acquire_shared(
                RootCacheDomain::Callgraph,
                &free_cache_dir,
                "free",
                &free_cache_dir,
            )
            .map_err(|error| error.to_string())
            .and_then(|lease| lease.ok_or_else(|| "writer lease unexpectedly denied".to_string()));
            free_tx.send(result).unwrap();
        });
        let free_lease = free_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("free root should not wait behind another root's acquisition")
            .expect("free root should acquire while another root is in acquisition");
        assert!(free_lease.verify().unwrap());
        free_handle
            .join()
            .expect("free root thread should not panic");

        release_tx.send(()).unwrap();
        let blocked_lease = blocked_handle
            .join()
            .expect("blocked root thread should not panic")
            .expect("blocked root should acquire after release");
        assert!(blocked_lease.verify().unwrap());
    }

    #[test]
    fn writer_lease_acquire_shared_reuses_single_process_lease_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("inspect").join("project");
        configure_artifact_access(&cache_dir, "project", false);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let barrier = std::sync::Arc::clone(&barrier);
            let cache_dir = cache_dir.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                WriterLease::acquire_shared(
                    RootCacheDomain::Inspect,
                    &cache_dir,
                    "project",
                    &cache_dir,
                )
                .map_err(|error| error.to_string())
                .and_then(|lease| {
                    lease.ok_or_else(|| "writer lease unexpectedly denied".to_string())
                })
            }));
        }

        let leases = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        let epoch = leases[0].epoch().to_string();
        let path = leases[0].path().to_path_buf();
        for lease in &leases {
            assert_eq!(lease.epoch(), epoch);
            assert_eq!(lease.path(), path.as_path());
            assert!(lease.verify().unwrap());
        }
    }

    #[test]
    fn zero_timeout_writer_lease_keeps_one_shot_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("inspect").join("project");
        configure_artifact_access(&cache_dir, "project", false);
        let clock = fs_lock::retry_clock::Guard::install(Duration::ZERO);
        let lease = WriterLease::acquire_shared_with_timeout(
            RootCacheDomain::Inspect,
            &cache_dir,
            "project",
            &cache_dir,
            Duration::ZERO,
        )
        .expect("zero timeout still attempts the filesystem lease once")
        .expect("writer access allowed");
        assert!(lease.verify().unwrap());
        assert_eq!(clock.observations(), (Duration::ZERO, vec![], 1));
    }

    #[test]
    fn nfs_guard_test_seam_fails_closed() {
        set_force_network_fs_for_test(true);
        assert!(!storage_allows_root_keyed(Path::new(".")).unwrap());
        set_force_network_fs_for_test(false);
    }

    #[test]
    fn audit_growth_boot_time_cache_reads_its_source_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proc-stat-fixture");
        fs::write(&path, "btime 1234\n").unwrap();
        let cache = OnceLock::new();
        let reads = std::cell::Cell::new(0);
        let first = cached_boot_time_secs(&cache, || {
            reads.set(reads.get() + 1);
            fs::read_to_string(&path)
                .ok()?
                .lines()
                .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
        });
        let second = cached_boot_time_secs(&cache, || {
            reads.set(reads.get() + 1);
            fs::read_to_string(&path)
                .ok()?
                .lines()
                .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
        });
        assert_eq!(first, Some(1234));
        assert_eq!(second, first);
        assert_eq!(
            reads.get(),
            1,
            "/proc/stat's equivalent source is read once"
        );
    }
}
