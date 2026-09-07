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
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fs_lock;

static MARKER_SEQ: AtomicU64 = AtomicU64::new(0);
static LIVE_SCOPE_KEYS: OnceLock<Mutex<HashMap<(PathBuf, String), usize>>> = OnceLock::new();

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

/// Track scope keys belonging to roots currently bound in this process. The
/// inspect sweep snapshots this registry at publication time so a live actor's
/// cache cannot be mistaken for a reclaimed worktree cache.
fn live_scope_keys() -> &'static Mutex<HashMap<(PathBuf, String), usize>> {
    LIVE_SCOPE_KEYS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register_live_scope(storage_root: &Path, project_root: &Path) {
    let storage_root = canonical_root(storage_root);
    let scope_key = crate::path_identity::project_scope_key(project_root);
    let mut scopes = match live_scope_keys().lock() {
        Ok(scopes) => scopes,
        Err(poisoned) => poisoned.into_inner(),
    };
    *scopes.entry((storage_root, scope_key)).or_default() += 1;
}

pub(crate) fn unregister_live_scope(storage_root: &Path, project_root: &Path) {
    let storage_root = canonical_root(storage_root);
    let scope_key = crate::path_identity::project_scope_key(project_root);
    let mut scopes = match live_scope_keys().lock() {
        Ok(scopes) => scopes,
        Err(poisoned) => poisoned.into_inner(),
    };
    let key = (storage_root, scope_key);
    if let Some(count) = scopes.get_mut(&key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
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
        .filter(|((root, _), count)| root == &storage_root && **count > 0)
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

fn canonical_root(project_root: &Path) -> PathBuf {
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
        Self::acquire_shared_inner(domain, cache_dir, key, project_root, None)
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
        Self::acquire_shared_inner(domain, cache_dir, key, project_root, Some(timeout))
    }

    fn acquire_shared_inner(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
        project_root: &Path,
        timeout: Option<Duration>,
    ) -> Result<Option<Arc<Self>>, fs_lock::AcquireError> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
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
                        let now = Instant::now();
                        if now >= deadline {
                            return Err(fs_lock::AcquireError::Timeout);
                        }
                        std::thread::sleep(
                            Duration::from_millis(10).min(deadline.saturating_duration_since(now)),
                        );
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

        let filesystem_timeout = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::ZERO);
        let lease = Arc::new(Self::acquire(domain, cache_dir, key, filesystem_timeout)?);
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
        timeout: Duration,
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
        fs::create_dir_all(cache_dir)?;
        let guard = fs_lock::try_acquire(&writer_lease_path(cache_dir), timeout)?;
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
}

#[derive(Debug)]
pub struct ReadMarker {
    path: PathBuf,
    metadata: ReadMarkerMetadata,
    last_touched_at_ms: AtomicU64,
}

impl ReadMarker {
    pub fn create(cache_dir: &Path, generation_label: &str) -> io::Result<Self> {
        let metadata = ReadMarkerMetadata {
            pid: std::process::id(),
            hostname: current_hostname(),
            created_at_ms: now_ms(),
        };
        let dir = read_marker_dir(cache_dir, generation_label);
        fs::create_dir_all(&dir)?;
        let seq = MARKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{}.{}.{}.{}.json",
            metadata.pid,
            sanitize_marker_component(&metadata.hostname),
            metadata.created_at_ms,
            seq
        ));
        write_marker_file(&path, &metadata)?;
        let last_touched_at_ms = AtomicU64::new(metadata.created_at_ms);
        Ok(Self {
            path,
            metadata,
            last_touched_at_ms,
        })
    }

    pub fn touch(&self) -> io::Result<()> {
        write_marker_file(&self.path, &self.metadata)?;
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
        let _ = fs::remove_file(&self.path);
        fs_lock::sync_parent(&self.path);
    }
}

pub fn read_marker_dir(cache_dir: &Path, generation_label: &str) -> PathBuf {
    cache_dir.join("readers").join(generation_label)
}

/// Sweep every generation marker below a cache directory, reusing the same
/// PID-authoritative and cross-host-TTL liveness rules as generation GC.
pub(crate) fn sweep_all_read_markers(cache_dir: &Path) -> ReadMarkerSweep {
    let readers = cache_dir.join("readers");
    let entries = match fs::read_dir(&readers) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ReadMarkerSweep::default(),
        Err(_) => {
            return ReadMarkerSweep {
                protected: true,
                removed_stale: 0,
            }
        }
    };

    let mut sweep = ReadMarkerSweep::default();
    for entry in entries {
        let Ok(entry) = entry else {
            sweep.protected = true;
            continue;
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                sweep.protected = true;
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(label) = path.file_name().and_then(|name| name.to_str()) else {
            sweep.protected = true;
            continue;
        };
        let generation = sweep_read_markers(cache_dir, label);
        sweep.protected |= generation.protected;
        sweep.removed_stale += generation.removed_stale;
    }
    sweep
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

fn read_marker_protection(
    cache_dir: &Path,
    generation_label: &str,
    remove_stale: bool,
) -> ReadMarkerSweep {
    let dir = read_marker_dir(cache_dir, generation_label);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ReadMarkerSweep::default(),
        Err(_) => {
            return ReadMarkerSweep {
                protected: true,
                removed_stale: 0,
            };
        }
    };

    let hostname = current_hostname();
    let now = now_ms();
    let mut sweep = ReadMarkerSweep::default();
    for entry in entries.flatten() {
        let path = entry.path();
        match marker_file_is_protected(&path, now, &hostname) {
            MarkerProtection::Protected => sweep.protected = true,
            MarkerProtection::Stale | MarkerProtection::Malformed => {
                if remove_stale && fs::remove_file(&path).is_ok() {
                    fs_lock::sync_parent(&path);
                    sweep.removed_stale += 1;
                }
            }
        }
    }
    sweep
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerProtection {
    Protected,
    Stale,
    Malformed,
}

fn marker_file_is_protected(path: &Path, now: u64, current_host: &str) -> MarkerProtection {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return MarkerProtection::Stale,
        Err(_) => return MarkerProtection::Protected,
    };
    let metadata: ReadMarkerMetadata = match serde_json::from_slice(&bytes) {
        Ok(metadata) => metadata,
        Err(_) => return MarkerProtection::Malformed,
    };
    if metadata.hostname != current_host {
        let Ok(file_metadata) = fs::metadata(path) else {
            return MarkerProtection::Protected;
        };
        let mtime_ms = file_metadata
            .modified()
            .ok()
            .map(system_time_ms)
            .unwrap_or(now);
        let age_ms = now.saturating_sub(mtime_ms);
        return if age_ms <= READ_MARKER_CROSS_HOST_STALE_MS {
            MarkerProtection::Protected
        } else {
            MarkerProtection::Stale
        };
    }

    if !fs_lock::process_alive(metadata.pid) {
        return MarkerProtection::Stale;
    }
    if marker_matches_live_process_instance(&metadata) {
        MarkerProtection::Protected
    } else {
        MarkerProtection::Stale
    }
}

fn marker_matches_live_process_instance(metadata: &ReadMarkerMetadata) -> bool {
    // Same-host PID liveness is authoritative for the process instance. When the
    // OS can tell us the live PID started after this marker was created, the PID
    // has been reused and the marker belongs to a dead prior process; otherwise
    // a live PID protects the marker without consulting marker mtime.
    process_start_time_ms(metadata.pid)
        .map(|started_at_ms| {
            started_at_ms
                <= metadata
                    .created_at_ms
                    .saturating_add(PROCESS_START_TIME_GRACE_MS)
        })
        .unwrap_or(true)
}

fn write_marker_file(path: &Path, metadata: &ReadMarkerMetadata) -> io::Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("reader"),
        std::process::id(),
        now_nanos()
    ));
    let result = (|| {
        let mut file = open_private_file(&tmp)?;
        serde_json::to_writer(&mut file, metadata).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs_lock::rename_over(&tmp, path)?;
        fs_lock::sync_parent(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn open_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
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

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
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
pub(crate) fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields = after_comm.split_whitespace().collect::<Vec<_>>();
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    let boot_time_secs = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())?;
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

#[cfg(target_os = "macos")]
pub(crate) fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let info_size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            info_size,
        )
    };
    if bytes != info_size {
        return None;
    }
    Some(
        info.pbi_start_tvsec
            .saturating_mul(1_000)
            .saturating_add(info.pbi_start_tvusec / 1_000),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }
    let _ = pid;
    None
}

#[cfg(unix)]
fn current_hostname() -> String {
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
    fn nfs_guard_test_seam_fails_closed() {
        set_force_network_fs_for_test(true);
        assert!(!storage_allows_root_keyed(Path::new(".")).unwrap());
        set_force_network_fs_for_test(false);
    }
}
