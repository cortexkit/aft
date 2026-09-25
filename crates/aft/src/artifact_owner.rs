use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fs_lock;

const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ArtifactOwnerManifest {
    pub schema_version: u32,
    pub project_scope_key: String,
    pub checkout_path: String,
    pub git_common_dir: Option<String>,
    pub pid: u32,
    pub hostname: String,
    pub created_at_ms: u64,
    pub heartbeat_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOwnerMode {
    Owner,
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArtifactOwnerStatus {
    pub mode: ArtifactOwnerMode,
    pub project_key: String,
    pub manifest_path: String,
    pub owner_project_scope_key: String,
    pub owner_checkout_path: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ArtifactOwnerLease {
    path: PathBuf,
    manifest: ArtifactOwnerManifest,
    last_heartbeat_ms: u64,
}

#[derive(Debug)]
pub struct ArtifactOwnerClaim {
    pub status: ArtifactOwnerStatus,
    pub lease: Option<ArtifactOwnerLease>,
}

#[derive(Debug)]
pub struct ArtifactOwnerLeaseRegistration {
    id: u64,
    state: Arc<HeartbeatState>,
}

#[derive(Debug)]
struct HeartbeatState {
    registry: Mutex<HeartbeatRegistry>,
    next_id: AtomicU64,
    thread_started: AtomicBool,
    shutdown: AtomicBool,
    wake_tx: crossbeam_channel::Sender<()>,
    wake_rx: crossbeam_channel::Receiver<()>,
}

#[derive(Debug, Default)]
struct HeartbeatRegistry {
    leases: BTreeMap<u64, ArtifactOwnerLease>,
    warned_failures: BTreeSet<PathBuf>,
}

static HEARTBEAT_STATE: OnceLock<Arc<HeartbeatState>> = OnceLock::new();

#[cfg(test)]
fn artifact_owner_test_mutex() -> &'static Mutex<()> {
    static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    MUTEX.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
pub(crate) fn artifact_owner_test_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::test_env::lock_test_mutex(artifact_owner_test_mutex())
}

/// Route linked worktrees to borrowing before any same-family owner claim.
/// `is_linked_worktree` is the configure-time Git topology result; ownership
/// code must not probe `.git` again while opening an artifact.
pub fn claim_or_open_read_only(
    storage_dir: Option<&Path>,
    project_root: &Path,
    project_key: &str,
    project_scope_key: &str,
    is_linked_worktree: bool,
    git_common_dir: Option<&Path>,
) -> io::Result<ArtifactOwnerClaim> {
    if is_linked_worktree {
        return Ok(open_read_only_borrow(
            storage_dir,
            project_root,
            project_key,
            project_scope_key,
        ));
    }

    let manifest_dir = resolve_manifest_dir(storage_dir, project_root, project_key);
    fs::create_dir_all(&manifest_dir)?;
    let path = manifest_dir.join("owner.json");
    let checkout_path = project_root.display().to_string();
    let git_common_dir = git_common_dir.map(|path| path.display().to_string());

    loop {
        match read_manifest(&path) {
            Ok(existing) => {
                let same_checkout = existing.project_scope_key == project_scope_key;
                let same_git_family = existing
                    .git_common_dir
                    .as_deref()
                    .zip(git_common_dir.as_deref())
                    .is_some_and(|(existing, current)| existing == current);
                if same_checkout || same_git_family {
                    #[cfg(test)]
                    run_before_owner_write_hook(&manifest_dir);
                    match write_owner_manifest(
                        &path,
                        project_key,
                        project_scope_key,
                        &checkout_path,
                        git_common_dir.as_deref(),
                    ) {
                        // The orphaned-manifest sweep removed the key
                        // directory; recreate it and claim again.
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            fs::create_dir_all(&manifest_dir)?;
                            continue;
                        }
                        result => return result,
                    }
                }

                if manifest_owner_alive(&existing) {
                    let note = read_only_borrow_note(&existing.checkout_path);
                    return Ok(ArtifactOwnerClaim {
                        status: ArtifactOwnerStatus {
                            mode: ArtifactOwnerMode::ReadOnly,
                            project_key: project_key.to_string(),
                            manifest_path: path.display().to_string(),
                            owner_project_scope_key: existing.project_scope_key,
                            owner_checkout_path: existing.checkout_path,
                            note: Some(note),
                        },
                        lease: None,
                    });
                }

                if reclaim_manifest_if_unchanged(&path, &existing)? {
                    continue;
                }
            }
            Err(ReadManifestError::NotFound) => {
                #[cfg(test)]
                run_before_owner_write_hook(&manifest_dir);
                match create_owner_manifest(
                    &path,
                    project_key,
                    project_scope_key,
                    &checkout_path,
                    git_common_dir.as_deref(),
                ) {
                    Ok(claim) => return Ok(claim),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    // The orphaned-manifest sweep removes empty key
                    // directories; recreate it and try again.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir_all(&manifest_dir)?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(ReadManifestError::Malformed) => {
                let _ = fs::remove_file(&path);
                continue;
            }
            Err(ReadManifestError::Io(error)) => return Err(error),
        }
    }
}

pub fn open_read_only_borrow(
    storage_dir: Option<&Path>,
    project_root: &Path,
    project_key: &str,
    project_scope_key: &str,
) -> ArtifactOwnerClaim {
    let manifest_dir = resolve_manifest_dir(storage_dir, project_root, project_key);
    let path = manifest_dir.join("owner.json");
    let fallback_checkout = project_root.display().to_string();

    let (owner_project_scope_key, owner_checkout_path, note) = match read_manifest(&path) {
        Ok(existing) => {
            let note = read_only_borrow_note(&existing.checkout_path);
            (existing.project_scope_key, existing.checkout_path, note)
        }
        Err(ReadManifestError::NotFound) => (
            project_scope_key.to_string(),
            fallback_checkout.clone(),
            "sharing the repo index family; waiting for the main checkout to publish shared artifacts"
                .to_string(),
        ),
        Err(ReadManifestError::Malformed) => (
            project_scope_key.to_string(),
            fallback_checkout.clone(),
            "sharing the repo index family; owner manifest is malformed; not repairing it from a linked worktree"
                .to_string(),
        ),
        Err(ReadManifestError::Io(error)) => (
            project_scope_key.to_string(),
            fallback_checkout.clone(),
            format!(
                "sharing the repo index family; failed to inspect owner manifest: {error}"
            ),
        ),
    };

    ArtifactOwnerClaim {
        status: ArtifactOwnerStatus {
            mode: ArtifactOwnerMode::ReadOnly,
            project_key: project_key.to_string(),
            manifest_path: path.display().to_string(),
            owner_project_scope_key,
            owner_checkout_path,
            note: Some(note),
        },
        lease: None,
    }
}

pub fn register_heartbeat(lease: ArtifactOwnerLease) -> ArtifactOwnerLeaseRegistration {
    let state = heartbeat_state();
    start_heartbeat_thread(&state);
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    {
        let mut registry = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.warned_failures.remove(&lease.path);
        registry.leases.insert(id, lease);
    }
    wake_heartbeat_thread(&state);
    ArtifactOwnerLeaseRegistration { id, state }
}

pub fn shutdown_heartbeat_thread() {
    if let Some(state) = HEARTBEAT_STATE.get() {
        state.shutdown.store(true, Ordering::SeqCst);
        wake_heartbeat_thread(state);
    }
}

impl Drop for ArtifactOwnerLeaseRegistration {
    fn drop(&mut self) {
        let removed = {
            let mut registry = self
                .state
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let removed = registry.leases.remove(&self.id);
            if let Some(lease) = &removed {
                registry.warned_failures.remove(&lease.path);
            }
            removed
        };
        if removed.is_some() {
            wake_heartbeat_thread(&self.state);
        }
    }
}

fn heartbeat_state() -> Arc<HeartbeatState> {
    HEARTBEAT_STATE
        .get_or_init(|| {
            let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
            Arc::new(HeartbeatState {
                registry: Mutex::new(HeartbeatRegistry::default()),
                next_id: AtomicU64::new(1),
                thread_started: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                wake_tx,
                wake_rx,
            })
        })
        .clone()
}

fn start_heartbeat_thread(state: &Arc<HeartbeatState>) {
    if state.thread_started.swap(true, Ordering::SeqCst) {
        return;
    }

    let state = Arc::clone(state);
    thread::spawn(move || heartbeat_thread_loop(state));
}

fn heartbeat_thread_loop(state: Arc<HeartbeatState>) {
    let ticker = crossbeam_channel::tick(Duration::from_millis(heartbeat_interval_ms()));
    while !state.shutdown.load(Ordering::SeqCst) {
        if !heartbeat_registry_has_leases(&state) {
            if state.wake_rx.recv().is_err() {
                break;
            }
            if state.shutdown.load(Ordering::SeqCst) {
                break;
            }
            heartbeat_registered_leases(&state);
            continue;
        }

        crossbeam_channel::select! {
            recv(ticker) -> tick => {
                if tick.is_err() {
                    break;
                }
            }
            recv(state.wake_rx) -> _ => {}
        }

        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }

        heartbeat_registered_leases(&state);
    }
}

fn heartbeat_registry_has_leases(state: &HeartbeatState) -> bool {
    state
        .registry
        .lock()
        .map(|registry| !registry.leases.is_empty())
        .unwrap_or(false)
}

fn heartbeat_registered_leases(state: &HeartbeatState) {
    let leases = {
        let registry = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .leases
            .iter()
            .map(|(id, lease)| (*id, lease.clone()))
            .collect::<Vec<_>>()
    };

    for (id, mut lease) in leases {
        let path = lease.path.clone();
        match lease.try_heartbeat_if_due() {
            Ok(false) => {}
            Ok(true) => {
                let mut registry = state
                    .registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(current) = registry.leases.get_mut(&id) {
                    current.manifest.heartbeat_at_ms = lease.manifest.heartbeat_at_ms;
                    current.last_heartbeat_ms = lease.last_heartbeat_ms;
                }
            }
            Err(error) => {
                let should_warn = {
                    let mut registry = state
                        .registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    registry.warned_failures.insert(path.clone())
                };
                if should_warn {
                    crate::slog_warn!(
                        "artifact owner heartbeat failed for {}: {}",
                        path.display(),
                        error
                    );
                }
            }
        }
    }
}

fn wake_heartbeat_thread(state: &HeartbeatState) {
    let _ = state.wake_tx.try_send(());
}

impl ArtifactOwnerLease {
    pub fn heartbeat_if_due(&mut self) {
        let _ = self.try_heartbeat_if_due();
    }

    fn try_heartbeat_if_due(&mut self) -> io::Result<bool> {
        let now = now_ms();
        if now.saturating_sub(self.last_heartbeat_ms) < heartbeat_interval_ms() {
            return Ok(false);
        }
        let previous = self.manifest.clone();
        self.manifest.heartbeat_at_ms = now;
        if let Err(error) = heartbeat_manifest(&self.path, &previous, &self.manifest) {
            self.manifest = previous;
            return Err(error);
        }
        self.last_heartbeat_ms = now;
        Ok(true)
    }
}

/// Write a heartbeat into the owner manifest.
///
/// Durability: the heartbeat only has to be visible to other processes, so it
/// never fsyncs the file or its directory. After a crash the manifest simply
/// carries an older heartbeat and looks stale sooner, which the ownership
/// rules already handle. Claiming or reclaiming ownership still goes through
/// the fsyncing `create_owner_manifest` / `atomic_write_manifest`.
///
/// When the file still holds exactly the manifest this lease last wrote, the
/// new bytes have the same length (only timestamp digits change), so they are
/// written in place: no new inode per beat and no moment where the file is
/// missing or short. Anything else (the file is gone, was rewritten by another
/// claim, or the length would change) falls back to the previous behaviour of
/// replacing the file through a temp-file rename, minus the fsyncs.
fn heartbeat_manifest(
    path: &Path,
    previous: &ArtifactOwnerManifest,
    next: &ArtifactOwnerManifest,
) -> io::Result<()> {
    let next_bytes = manifest_bytes(next)?;
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(mut file) => {
            let mut current = Vec::new();
            file.read_to_end(&mut current)?;
            let unchanged = serde_json::from_slice::<ArtifactOwnerManifest>(&current)
                .is_ok_and(|on_disk| on_disk == *previous);
            if unchanged && current.len() == next_bytes.len() {
                file.seek(SeekFrom::Start(0))?;
                return write_manifest_bytes(&mut file, &next_bytes);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    replace_manifest_unsynced(path, &next_bytes)
}

fn replace_manifest_unsynced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut file = File::create(&tmp)?;
        fs_lock::io_ledger::record(|ledger| ledger.new_files += 1);
        write_manifest_bytes(&mut file, bytes)?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// Human-facing note for a read-only borrow of another checkout's index family.
/// This is a shared-index arrangement, not a degraded mode.
fn read_only_borrow_note(owner_checkout: &str) -> String {
    format!("sharing the repo index family owned by {owner_checkout} (read-only borrow)")
}

fn create_owner_manifest(
    path: &Path,
    project_key: &str,
    project_scope_key: &str,
    checkout_path: &str,
    git_common_dir: Option<&str>,
) -> io::Result<ArtifactOwnerClaim> {
    let manifest = new_manifest(project_scope_key, checkout_path, git_common_dir);
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    fs_lock::io_ledger::record(|ledger| ledger.new_files += 1);
    write_manifest_to_file(&mut file, &manifest)?;
    fs_lock::sync_lease_file(&file)?;
    sync_parent(path);
    Ok(owner_claim(path, project_key, manifest))
}

fn write_owner_manifest(
    path: &Path,
    project_key: &str,
    project_scope_key: &str,
    checkout_path: &str,
    git_common_dir: Option<&str>,
) -> io::Result<ArtifactOwnerClaim> {
    let manifest = new_manifest(project_scope_key, checkout_path, git_common_dir);
    atomic_write_manifest(path, &manifest)?;
    Ok(owner_claim(path, project_key, manifest))
}

fn owner_claim(
    path: &Path,
    project_key: &str,
    manifest: ArtifactOwnerManifest,
) -> ArtifactOwnerClaim {
    let last_heartbeat_ms = manifest.heartbeat_at_ms;
    ArtifactOwnerClaim {
        status: ArtifactOwnerStatus {
            mode: ArtifactOwnerMode::Owner,
            project_key: project_key.to_string(),
            manifest_path: path.display().to_string(),
            owner_project_scope_key: manifest.project_scope_key.clone(),
            owner_checkout_path: manifest.checkout_path.clone(),
            note: None,
        },
        lease: Some(ArtifactOwnerLease {
            path: path.to_path_buf(),
            manifest,
            last_heartbeat_ms,
        }),
    }
}

fn new_manifest(
    project_scope_key: &str,
    checkout_path: &str,
    git_common_dir: Option<&str>,
) -> ArtifactOwnerManifest {
    let now = now_ms();
    ArtifactOwnerManifest {
        schema_version: SCHEMA_VERSION,
        project_scope_key: project_scope_key.to_string(),
        checkout_path: checkout_path.to_string(),
        git_common_dir: git_common_dir.map(str::to_string),
        pid: std::process::id(),
        hostname: current_hostname(),
        created_at_ms: now,
        heartbeat_at_ms: now,
    }
}

fn manifest_owner_alive(manifest: &ArtifactOwnerManifest) -> bool {
    let now = now_ms();
    let since_heartbeat = now.saturating_sub(manifest.heartbeat_at_ms);
    if manifest.hostname != current_hostname() {
        return since_heartbeat <= fs_lock::STALE_HEARTBEAT_MS.saturating_mul(5);
    }
    process_alive(manifest.pid)
}

fn reclaim_manifest_if_unchanged(path: &Path, judged: &ArtifactOwnerManifest) -> io::Result<bool> {
    match read_manifest(path) {
        Ok(current)
            if current.pid == judged.pid
                && current.hostname == judged.hostname
                && current.created_at_ms == judged.created_at_ms =>
        {
            fs::remove_file(path)?;
            sync_parent(path);
            Ok(true)
        }
        Ok(_) | Err(ReadManifestError::NotFound) | Err(ReadManifestError::Malformed) => Ok(false),
        Err(ReadManifestError::Io(error)) => Err(error),
    }
}

#[derive(Debug)]
enum ReadManifestError {
    NotFound,
    Io(io::Error),
    Malformed,
}

/// Parse an owner manifest. An empty or partial file (for example one left by
/// a crash after an unsynced heartbeat rename) is `Malformed`: the claim path
/// removes it and claims afresh, it never reads as a live owner.
fn read_manifest(path: &Path) -> Result<ArtifactOwnerManifest, ReadManifestError> {
    let bytes = fs_lock::read_lease_settled(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ReadManifestError::NotFound
        } else {
            ReadManifestError::Io(error)
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|_| ReadManifestError::Malformed)
}

fn atomic_write_manifest(path: &Path, manifest: &ArtifactOwnerManifest) -> io::Result<()> {
    let tmp = temp_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut file = File::create(&tmp)?;
        fs_lock::io_ledger::record(|ledger| ledger.new_files += 1);
        write_manifest_to_file(&mut file, manifest)?;
        fs_lock::sync_lease_file(&file)?;
        fs::rename(&tmp, path)?;
        sync_parent(path);
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn manifest_bytes(manifest: &ArtifactOwnerManifest) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(manifest).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_manifest_to_file(file: &mut File, manifest: &ArtifactOwnerManifest) -> io::Result<()> {
    write_manifest_bytes(file, &manifest_bytes(manifest)?)
}

fn write_manifest_bytes(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    fs_lock::io_ledger::record(|ledger| ledger.bytes_written += bytes.len() as u64);
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    path.with_extension(format!("json.tmp.{}.{}", std::process::id(), now))
}

fn resolve_manifest_dir(
    storage_dir: Option<&Path>,
    _project_root: &Path,
    project_key: &str,
) -> PathBuf {
    // Ownership manifests must use the same root as indexes and leases. Resolve
    // the environment override here instead of maintaining a cache-only branch.
    owner_manifests_root(&crate::bash_background::storage_dir(storage_dir)).join(project_key)
}

fn owner_manifests_root(storage_root: &Path) -> PathBuf {
    storage_root.join("artifact-owners")
}

/// Directory entries under `artifact-owners/` examined per pass. The limit is
/// applied to the directory iterator itself (after skipping to the resume
/// offset), so a storage root with thousands of project keys costs at most
/// this many entries, and manifest reads, per pass.
const OWNER_REAP_SCAN_LIMIT: usize = 512;

/// A storage root is swept at most once per this interval per process. The
/// sweep runs from every configure tail, and after a daemon restart dozens of
/// roots configure at once; without the interval each of them would walk the
/// same directory.
const OWNER_REAP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A manifest must also have gone this long without a heartbeat before it is
/// reaped. A live owner rewrites the heartbeat every few seconds, so this only
/// keeps a manifest whose checkout was deleted moments ago (and whose owner may
/// still be shutting down) out of the sweep.
const OWNER_REAP_MIN_HEARTBEAT_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Per storage root: when the last pass was claimed, and the directory offset
/// the next pass resumes from.
#[derive(Debug, Default)]
struct OwnerReapState {
    last_run: Option<std::time::Instant>,
    next_offset: usize,
}

static OWNER_REAP_STATE: OnceLock<Mutex<std::collections::HashMap<PathBuf, OwnerReapState>>> =
    OnceLock::new();

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OwnerReapSummary {
    /// Directory entries taken from the iterator in this pass.
    pub(crate) examined: usize,
    /// Owner manifests removed in this pass.
    pub(crate) removed: usize,
    /// Offset the next pass skips to; 0 once a pass reached the end.
    pub(crate) next_offset: usize,
}

/// Remove owner manifests whose checkout no longer exists.
///
/// Every project key gets an `artifact-owners/<key>/owner.json`, and nothing
/// else removes them, so without this sweep the directory grows by one entry
/// for every checkout ever opened. Returns `None` when another caller already
/// ran a pass for this storage root within `OWNER_REAP_INTERVAL`.
pub(crate) fn sweep_orphaned_owner_manifests(storage_root: &Path) -> Option<OwnerReapSummary> {
    sweep_orphaned_owner_manifests_throttled(
        storage_root,
        OWNER_REAP_SCAN_LIMIT,
        OWNER_REAP_INTERVAL,
        now_ms(),
    )
}

fn sweep_orphaned_owner_manifests_throttled(
    storage_root: &Path,
    scan_limit: usize,
    interval: Duration,
    now: u64,
) -> Option<OwnerReapSummary> {
    let states = OWNER_REAP_STATE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    // Claim the pass and read the resume offset under one lock, so two
    // configure tails for the same storage root cannot both run it.
    let offset = {
        let mut states = states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(storage_root.to_path_buf()).or_default();
        let started = std::time::Instant::now();
        if state
            .last_run
            .is_some_and(|last_run| started.saturating_duration_since(last_run) < interval)
        {
            return None;
        }
        state.last_run = Some(started);
        state.next_offset
    };

    let summary = reap_owner_manifests_pass(storage_root, offset, scan_limit, now);
    if let Ok(mut states) = states.lock() {
        if let Some(state) = states.get_mut(storage_root) {
            state.next_offset = summary.next_offset;
        }
    }
    crate::slog_info!(
        "artifact owner cleanup: root={} examined={} removed={} next_offset={}",
        owner_manifests_root(storage_root).display(),
        summary.examined,
        summary.removed,
        summary.next_offset
    );
    Some(summary)
}

/// One bounded pass: skip `offset` directory entries, examine at most
/// `scan_limit`, and report where the next pass should resume. A pass that
/// reaches the end of the directory wraps the next offset to 0. Key
/// directories removed in this pass are subtracted from the offset, because
/// the entries after them move up by that many positions.
fn reap_owner_manifests_pass(
    storage_root: &Path,
    offset: usize,
    scan_limit: usize,
    now: u64,
) -> OwnerReapSummary {
    let root = owner_manifests_root(storage_root);
    let Ok(entries) = fs::read_dir(&root) else {
        return OwnerReapSummary::default();
    };
    let mut summary = OwnerReapSummary::default();
    let mut removed_dirs = 0;
    for entry in entries.skip(offset).take(scan_limit) {
        summary.examined += 1;
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let dir = entry.path();
        let path = dir.join("owner.json");
        match read_manifest(&path) {
            Ok(manifest) if owner_manifest_is_orphaned(&manifest, now) => {
                // Re-checks the owner identity right before unlinking, so a
                // claim that replaced the manifest meanwhile is left alone.
                if matches!(reclaim_manifest_if_unchanged(&path, &manifest), Ok(true)) {
                    summary.removed += 1;
                    // Only succeeds once the directory is empty. A claim that
                    // races this recreates the directory and retries.
                    if fs::remove_dir(&dir).is_ok() {
                        removed_dirs += 1;
                    }
                }
            }
            _ => {}
        }
    }
    summary.next_offset = if summary.examined < scan_limit {
        0
    } else {
        (offset + summary.examined).saturating_sub(removed_dirs)
    };
    summary
}

/// A manifest is orphaned when its checkout is definitely gone (a failed
/// existence check, such as a permission error, does not count) and its
/// heartbeat is older than `OWNER_REAP_MIN_HEARTBEAT_AGE_MS`.
fn owner_manifest_is_orphaned(manifest: &ArtifactOwnerManifest, now: u64) -> bool {
    !manifest.checkout_path.is_empty()
        && matches!(Path::new(&manifest.checkout_path).try_exists(), Ok(false))
        && now.saturating_sub(manifest.heartbeat_at_ms) > OWNER_REAP_MIN_HEARTBEAT_AGE_MS
}

fn sync_parent(path: &Path) {
    fs_lock::sync_parent(path);
}

fn heartbeat_interval_ms() -> u64 {
    #[cfg(test)]
    if let Ok(raw) = std::env::var("AFT_TEST_ARTIFACT_OWNER_HEARTBEAT_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            if ms > 0 {
                return ms;
            }
        }
    }

    fs_lock::HEARTBEAT_INTERVAL_MS
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
    crate::environment::non_empty_var("HOSTNAME").unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(windows)]
fn current_hostname() -> String {
    crate::environment::non_empty_var("COMPUTERNAME")
        .or_else(|| crate::environment::non_empty_var("HOSTNAME"))
        .unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(not(any(unix, windows)))]
fn current_hostname() -> String {
    crate::environment::non_empty_var("HOSTNAME").unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        // Our own process: trivially alive. This is a real production case
        // (the daemon serves sibling checkouts of one repo as two roots in
        // one process) and probing our own PID through the OS is where the
        // probe can flake.
        return true;
    }
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        // Our own process: trivially alive. Also avoids the tasklist probe,
        // which can return empty output under loaded-runner contention and
        // misreport a live owner as dead (observed as sibling checkouts
        // stealing the artifact lease in CI).
        return true;
    }
    // PID 0 is the System Idle Process on Windows, so tasklist reports it as
    // running; treat it as dead like the Unix path does (it can never be an
    // AFT bridge).
    if pid == 0 {
        return false;
    }
    let filter = format!("PID eq {pid}");
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
    else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    !stdout.contains("No tasks are running") && stdout.contains(&format!("\"{pid}\""))
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
thread_local! {
    /// Test seam run just before a claim writes the owner manifest, so a test
    /// can remove the key directory at the exact point a concurrent sweep could.
    static BEFORE_OWNER_WRITE_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&Path)>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_before_owner_write_hook(manifest_dir: &Path) {
    BEFORE_OWNER_WRITE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(manifest_dir);
        }
    });
}

#[cfg(test)]
pub(crate) fn write_synthetic_manifest_for_test(
    storage_dir: &Path,
    project_root: &Path,
    project_key: &str,
    project_scope_key: &str,
    pid: u32,
    heartbeat_at_ms: u64,
) {
    write_synthetic_manifest_with_git_common_dir_for_test(
        storage_dir,
        project_root,
        project_key,
        project_scope_key,
        pid,
        heartbeat_at_ms,
        None,
    );
}

#[cfg(test)]
pub(crate) fn write_synthetic_manifest_with_git_common_dir_for_test(
    storage_dir: &Path,
    project_root: &Path,
    project_key: &str,
    project_scope_key: &str,
    pid: u32,
    heartbeat_at_ms: u64,
    git_common_dir: Option<&Path>,
) {
    let path =
        resolve_manifest_dir(Some(storage_dir), project_root, project_key).join("owner.json");
    write_synthetic_manifest_at_path_for_test(
        &path,
        project_root,
        project_scope_key,
        pid,
        heartbeat_at_ms,
        git_common_dir,
    );
}

#[cfg(test)]
fn write_synthetic_manifest_at_path_for_test(
    path: &Path,
    project_root: &Path,
    project_scope_key: &str,
    pid: u32,
    heartbeat_at_ms: u64,
    git_common_dir: Option<&Path>,
) {
    fs::create_dir_all(path.parent().expect("owner manifest parent")).unwrap();
    let now = now_ms();
    let manifest = ArtifactOwnerManifest {
        schema_version: SCHEMA_VERSION,
        project_scope_key: project_scope_key.to_string(),
        checkout_path: project_root.display().to_string(),
        git_common_dir: git_common_dir.map(|path| path.display().to_string()),
        pid,
        hostname: current_hostname(),
        created_at_ms: now,
        heartbeat_at_ms,
    };
    atomic_write_manifest(path, &manifest).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::{Arc, Mutex as StdMutex, OnceLock as StdOnceLock};
    use std::time::Instant;

    use serde_json::json;

    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};
    use crate::executor::{Executor, Lane};
    use crate::path_identity::ProjectRootId;
    use crate::protocol::Response;

    static HEARTBEAT_TEST_SERIAL: StdOnceLock<StdMutex<()>> = StdOnceLock::new();

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.take() {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn heartbeat_serial_guard() -> std::sync::MutexGuard<'static, ()> {
        HEARTBEAT_TEST_SERIAL
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_test_heartbeat_interval(ms: u64) -> EnvVarGuard {
        EnvVarGuard::set("AFT_TEST_ARTIFACT_OWNER_HEARTBEAT_MS", ms.to_string())
    }

    fn exited_owner_pid() -> u32 {
        #[cfg(windows)]
        let mut child = Command::new("cmd.exe")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn short-lived owner process");
        #[cfg(unix)]
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived owner process");
        #[cfg(not(any(unix, windows)))]
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived owner process");

        let pid = child.id();
        let status = child.wait().expect("wait for owner process exit");
        assert!(status.success(), "short-lived owner exited unsuccessfully");
        pid
    }

    fn claim_stale_owner(
        storage_dir: &Path,
        root: &Path,
    ) -> (ArtifactOwnerStatus, ArtifactOwnerLease) {
        fs::create_dir_all(root).unwrap();
        let mut claim =
            claim_or_open_read_only(Some(storage_dir), root, "shared-key", "scope", false, None)
                .unwrap();
        let lease = claim.lease.as_mut().expect("owner lease");
        lease.manifest.heartbeat_at_ms = 0;
        lease.last_heartbeat_ms = 0;
        atomic_write_manifest(&lease.path, &lease.manifest).unwrap();
        (claim.status, claim.lease.take().unwrap())
    }

    fn context_with_artifact_owner(
        status: ArtifactOwnerStatus,
        lease: ArtifactOwnerLease,
    ) -> AppContext {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.set_artifact_owner(Some(status), Some(lease));
        ctx
    }

    fn wait_for_heartbeat(path: &Path, after_ms: u64) -> ArtifactOwnerManifest {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let manifest = read_manifest(path).unwrap();
            if manifest.heartbeat_at_ms > after_ms {
                return manifest;
            }
            assert!(Instant::now() < deadline, "timed out waiting for heartbeat");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_heartbeat_stops(path: &Path) {
        // The heartbeat thread snapshots the lease list before writing, so
        // unregistration can race AT MOST ONE in-flight write (the loop is
        // serial). Re-baseline until two consecutive reads agree instead of
        // assuming instant quiescence; fail only if writes keep advancing
        // past the deadline (a genuinely un-stopped heartbeat).
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut baseline = read_manifest(path).unwrap().heartbeat_at_ms;
        loop {
            thread::sleep(Duration::from_millis(150));
            let current = read_manifest(path).unwrap().heartbeat_at_ms;
            if current == baseline {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "heartbeat kept advancing after release: {baseline} -> {current}"
            );
            baseline = current;
        }
    }

    fn claim_owner(storage_dir: &Path, root: &Path, key: &str) -> ArtifactOwnerLease {
        fs::create_dir_all(root).unwrap();
        claim_or_open_read_only(Some(storage_dir), root, key, key, false, None)
            .unwrap()
            .lease
            .expect("owner lease")
    }

    #[cfg(unix)]
    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).unwrap().ino()
    }

    /// Sixty seconds of owner-manifest heartbeats on several leases must not
    /// fsync anything or create a new file per beat. Totals are printed so the
    /// cost can be compared across changes.
    #[test]
    fn heartbeat_ledger_for_sixty_seconds_has_no_fsync_and_no_new_inodes() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        const LEASES: usize = 8;
        let beats = (60_000 / fs_lock::HEARTBEAT_INTERVAL_MS) as usize;
        let mut leases = (0..LEASES)
            .map(|index| {
                let key = format!("key-{index}");
                claim_owner(temp.path(), &temp.path().join(&key), &key)
            })
            .collect::<Vec<_>>();
        #[cfg(unix)]
        let inodes_before = leases.iter().map(|l| inode(&l.path)).collect::<Vec<_>>();
        let file_bytes: u64 = leases
            .iter()
            .map(|lease| fs::metadata(&lease.path).unwrap().len())
            .sum();

        let _ = fs_lock::io_ledger::take();
        for _ in 0..beats {
            for lease in &mut leases {
                lease.last_heartbeat_ms = 0;
                assert!(lease.try_heartbeat_if_due().unwrap());
            }
        }
        let ledger = fs_lock::io_ledger::take();
        eprintln!(
            "artifact owner heartbeat ledger: leases={LEASES} beats_per_lease={beats} {ledger:?}"
        );

        assert_eq!(
            ledger.file_syncs, 0,
            "heartbeats must not fsync the manifest"
        );
        assert_eq!(
            ledger.dir_syncs, 0,
            "heartbeats must not fsync the directory"
        );
        assert_eq!(ledger.new_files, 0, "heartbeats must rewrite in place");
        assert_eq!(ledger.bytes_written, file_bytes * beats as u64);
        #[cfg(unix)]
        assert_eq!(
            leases.iter().map(|l| inode(&l.path)).collect::<Vec<_>>(),
            inodes_before
        );
        for lease in &leases {
            assert_eq!(read_manifest(&lease.path).unwrap(), lease.manifest);
        }
    }

    /// Claiming, re-claiming and reclaiming an owner manifest keep full
    /// durability.
    #[test]
    fn owner_claim_and_reclaim_still_fsync() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");

        let _ = fs_lock::io_ledger::take();
        let lease = claim_owner(temp.path(), &root, "key");
        let created = fs_lock::io_ledger::take();
        assert_eq!(
            (created.file_syncs, created.dir_syncs, created.new_files),
            (1, 1, 1),
            "first claim: {created:?}"
        );

        let _ = claim_owner(temp.path(), &root, "key");
        let reclaimed = fs_lock::io_ledger::take();
        assert_eq!(
            (
                reclaimed.file_syncs,
                reclaimed.dir_syncs,
                reclaimed.new_files
            ),
            (1, 1, 1),
            "same-checkout re-claim: {reclaimed:?}"
        );

        let current = read_manifest(&lease.path).unwrap();
        assert!(reclaim_manifest_if_unchanged(&lease.path, &current).unwrap());
        let removed = fs_lock::io_ledger::take();
        assert_eq!(removed.dir_syncs, 1, "dead-owner removal: {removed:?}");
    }

    /// A crash after an unsynced heartbeat rename can leave a zero-length
    /// manifest. The claim path must treat it as stale and claim ownership.
    #[test]
    fn zero_length_manifest_is_treated_as_stale() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let path = resolve_manifest_dir(Some(temp.path()), &root, "key").join("owner.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"").unwrap();
        assert!(matches!(
            read_manifest(&path),
            Err(ReadManifestError::Malformed)
        ));

        let claim =
            claim_or_open_read_only(Some(temp.path()), &root, "key", "scope", false, None).unwrap();
        assert_eq!(claim.status.mode, ArtifactOwnerMode::Owner);
        assert_eq!(read_manifest(&path).unwrap().project_scope_key, "scope");
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    fn write_owner_manifest_for_reap(
        storage: &Path,
        key: &str,
        checkout: &Path,
        heartbeat_at_ms: u64,
    ) -> PathBuf {
        let path = owner_manifests_root(storage).join(key).join("owner.json");
        write_synthetic_manifest_at_path_for_test(
            &path,
            checkout,
            key,
            std::process::id(),
            heartbeat_at_ms,
            None,
        );
        path
    }

    /// Runs unthrottled passes the way the throttled wrapper does, carrying the
    /// resume offset between them.
    fn reap_passes(storage: &Path, limit: usize, now: u64, passes: usize) -> Vec<OwnerReapSummary> {
        let mut offset = 0;
        (0..passes)
            .map(|_| {
                let summary = reap_owner_manifests_pass(storage, offset, limit, now);
                offset = summary.next_offset;
                summary
            })
            .collect()
    }

    #[test]
    fn owner_reap_removes_only_manifests_whose_checkout_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let live_checkout = temp.path().join("live");
        fs::create_dir_all(&live_checkout).unwrap();
        let gone_checkout = temp.path().join("gone");
        let now = now_ms();
        let old = now - 2 * DAY_MS;

        let gone = write_owner_manifest_for_reap(&storage, "gone", &gone_checkout, old);
        let live = write_owner_manifest_for_reap(&storage, "live", &live_checkout, old);
        let recent = write_owner_manifest_for_reap(&storage, "recent", &gone_checkout, now - 1_000);
        let malformed = owner_manifests_root(&storage)
            .join("malformed")
            .join("owner.json");
        fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        fs::write(&malformed, b"").unwrap();

        let summary = reap_owner_manifests_pass(&storage, 0, 100, now);

        assert_eq!(
            summary,
            OwnerReapSummary {
                examined: 4,
                removed: 1,
                next_offset: 0
            }
        );
        assert!(
            !gone.exists(),
            "manifest for a deleted checkout must be reaped"
        );
        assert!(
            !gone.parent().unwrap().exists(),
            "the emptied key directory must be removed"
        );
        assert!(live.exists(), "manifest for an existing checkout must stay");
        assert!(recent.exists(), "recently heartbeated manifest must stay");
        assert!(
            malformed.exists(),
            "unparseable manifest must be left alone"
        );
    }

    #[test]
    fn owner_reap_is_bounded_by_the_scan_limit() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let gone_checkout = temp.path().join("gone");
        let now = now_ms();
        for index in 0..5 {
            write_owner_manifest_for_reap(
                &storage,
                &format!("gone-{index}"),
                &gone_checkout,
                now - 2 * DAY_MS,
            );
        }

        let passes = reap_passes(&storage, 2, now, 3);
        assert_eq!(
            passes.iter().map(|pass| pass.examined).collect::<Vec<_>>(),
            vec![2, 2, 1]
        );
        assert_eq!(
            passes.iter().map(|pass| pass.removed).collect::<Vec<_>>(),
            vec![2, 2, 1]
        );
        assert_eq!(
            fs::read_dir(owner_manifests_root(&storage))
                .unwrap()
                .count(),
            0
        );
    }

    /// More live entries than one pass can examine must not hide dead ones:
    /// the resume offset carries the scan past them, so every dead manifest
    /// is reaped within ceil(total / limit) + 1 passes, wherever the dead
    /// entries fall in directory order.
    #[test]
    fn owner_reap_resumes_past_live_entries_until_every_dead_one_is_reaped() {
        const LIMIT: usize = 4;
        const TOTAL: usize = 13;
        const DEAD: usize = 3;
        let max_passes = TOTAL.div_ceil(LIMIT) + 1;
        type PickDead = fn(&[PathBuf]) -> Vec<PathBuf>;
        let placements: [(&str, PickDead); 3] = [
            ("dead last", |order| order[order.len() - DEAD..].to_vec()),
            ("dead first", |order| order[..DEAD].to_vec()),
            ("dead scattered", |order| {
                vec![
                    order[0].clone(),
                    order[order.len() / 2].clone(),
                    order[order.len() - 1].clone(),
                ]
            }),
        ];

        for (placement, pick_dead) in placements {
            let temp = tempfile::tempdir().unwrap();
            let storage = temp.path().join("storage");
            let live_checkout = temp.path().join("live");
            fs::create_dir_all(&live_checkout).unwrap();
            let gone_checkout = temp.path().join("gone");
            let now = now_ms();
            let old = now - 2 * DAY_MS;
            for index in 0..TOTAL {
                write_owner_manifest_for_reap(
                    &storage,
                    &format!("key-{index:02}"),
                    &live_checkout,
                    old,
                );
            }
            // Choose the dead entries by the directory's own iteration order,
            // which is what the sweep walks.
            let order = fs::read_dir(owner_manifests_root(&storage))
                .unwrap()
                .map(|entry| entry.unwrap().path().join("owner.json"))
                .collect::<Vec<_>>();
            let dead = pick_dead(&order);
            for path in &dead {
                let key = path
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap();
                write_owner_manifest_for_reap(&storage, key, &gone_checkout, old);
            }

            let passes = reap_passes(&storage, LIMIT, now, max_passes);
            let removed: usize = passes.iter().map(|pass| pass.removed).sum();
            assert_eq!(removed, DEAD, "{placement}: {passes:?}");
            for path in &dead {
                assert!(
                    !path.exists(),
                    "{placement}: {} survived {passes:?}",
                    path.display()
                );
            }
            assert!(
                passes.iter().all(|pass| pass.examined <= LIMIT),
                "{placement}: {passes:?}"
            );
            assert_eq!(
                fs::read_dir(owner_manifests_root(&storage))
                    .unwrap()
                    .count(),
                TOTAL - DEAD,
                "{placement}: live manifests must stay"
            );
        }
    }

    /// A second call inside the interval examines nothing, even when there is
    /// now something to reap, and concurrent callers claim exactly one pass.
    #[test]
    fn owner_reap_runs_at_most_once_per_interval_per_storage_root() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let gone_checkout = temp.path().join("gone");
        let now = now_ms();
        let interval = Duration::from_secs(3600);
        let first =
            write_owner_manifest_for_reap(&storage, "first", &gone_checkout, now - 2 * DAY_MS);

        let summary = sweep_orphaned_owner_manifests_throttled(&storage, 512, interval, now)
            .expect("first call runs a pass");
        assert_eq!((summary.examined, summary.removed), (1, 1));
        assert!(!first.exists());

        let second =
            write_owner_manifest_for_reap(&storage, "second", &gone_checkout, now - 2 * DAY_MS);
        assert_eq!(
            sweep_orphaned_owner_manifests_throttled(&storage, 512, interval, now),
            None,
            "a call inside the interval must not examine anything"
        );
        assert!(second.exists(), "the throttled call must not reap");

        let other = temp.path().join("other-storage");
        write_owner_manifest_for_reap(&other, "other", &gone_checkout, now - 2 * DAY_MS);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let ran = (0..8)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let other = other.clone();
                thread::spawn(move || {
                    barrier.wait();
                    sweep_orphaned_owner_manifests_throttled(&other, 512, interval, now).is_some()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|ran| *ran)
            .count();
        assert_eq!(ran, 1, "concurrent callers must claim exactly one pass");
    }

    fn arm_remove_key_dir_once() {
        let mut fired = false;
        BEFORE_OWNER_WRITE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |dir: &Path| {
                if !fired {
                    fired = true;
                    fs::remove_dir_all(dir).unwrap();
                }
            }));
        });
    }

    fn disarm_owner_write_hook() {
        BEFORE_OWNER_WRITE_HOOK.with(|hook| *hook.borrow_mut() = None);
    }

    /// The sweep can remove a key directory between the claim creating it and
    /// writing the first manifest; the claim recreates it and succeeds.
    #[test]
    fn first_claim_survives_the_reap_removing_its_key_directory() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        arm_remove_key_dir_once();
        let lease = claim_owner(temp.path(), &root, "key");
        disarm_owner_write_hook();
        assert_eq!(read_manifest(&lease.path).unwrap(), lease.manifest);
    }

    /// Same race on a same-checkout re-claim, which rewrites the manifest
    /// through a temp file in the key directory.
    #[test]
    fn reclaim_survives_the_reap_removing_its_key_directory() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        claim_owner(temp.path(), &root, "key");
        arm_remove_key_dir_once();
        let result = claim_or_open_read_only(Some(temp.path()), &root, "key", "key", false, None);
        disarm_owner_write_hook();
        let lease = result
            .expect("re-claim must recreate the key directory")
            .lease
            .unwrap();
        assert_eq!(read_manifest(&lease.path).unwrap(), lease.manifest);
    }

    #[test]
    fn sibling_checkout_opens_read_only_while_owner_is_alive() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("owner");
        let sibling = temp.path().join("sibling");
        fs::create_dir_all(&owner).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let key = "shared-key";

        let first =
            claim_or_open_read_only(Some(temp.path()), &owner, key, "owner-scope", false, None)
                .unwrap();
        assert_eq!(first.status.mode, ArtifactOwnerMode::Owner);
        assert!(first.lease.is_some());

        let second = claim_or_open_read_only(
            Some(temp.path()),
            &sibling,
            key,
            "sibling-scope",
            false,
            None,
        )
        .unwrap();
        assert_eq!(second.status.mode, ArtifactOwnerMode::ReadOnly);
        let note = second.status.note.unwrap();
        assert!(
            note.contains("sharing the repo index family owned by"),
            "{note}"
        );
        assert!(note.contains("read-only borrow"), "{note}");
        assert!(second.lease.is_none());
    }

    #[test]
    fn same_checkout_reconfigure_reclaims_idempotently() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let key = "shared-key";

        let first =
            claim_or_open_read_only(Some(temp.path()), &root, key, "scope", false, None).unwrap();
        let second =
            claim_or_open_read_only(Some(temp.path()), &root, key, "scope", false, None).unwrap();

        assert_eq!(first.status.mode, ArtifactOwnerMode::Owner);
        assert_eq!(second.status.mode, ArtifactOwnerMode::Owner);
        assert!(second.lease.is_some());
    }

    #[test]
    fn dead_owner_is_reclaimed_by_different_checkout() {
        let _artifact_guard = artifact_owner_test_lock();
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let configured_storage = temp.path().join("configured-storage");
        let redirected_storage = temp.path().join("redirected-storage");
        let owner = temp.path().join("owner");
        let sibling = temp.path().join("sibling");
        fs::create_dir_all(&owner).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let key = "shared-key";
        let _storage_override = EnvVarGuard::set("AFT_STORAGE_DIR", redirected_storage.as_os_str());

        // Capture the override-resolved path once. The fixture writes and reads
        // only this path, while the lock keeps production's later resolution on
        // the same root.
        let manifest_path =
            resolve_manifest_dir(Some(&configured_storage), &owner, key).join("owner.json");
        let assumed_manifest_path = configured_storage
            .join("artifact-owners")
            .join(key)
            .join("owner.json");
        assert_ne!(manifest_path, assumed_manifest_path);
        assert!(matches!(
            read_manifest(&assumed_manifest_path),
            Err(ReadManifestError::NotFound)
        ));

        let exited_owner_pid = exited_owner_pid();
        write_synthetic_manifest_at_path_for_test(
            &manifest_path,
            &owner,
            "owner-scope",
            exited_owner_pid,
            0,
            None,
        );

        // The real child has already reported its terminal status. Re-read the
        // unchanged manifest from the captured root before judging its owner.
        let judged = read_manifest(&manifest_path).expect("owner manifest from resolved storage");
        assert_eq!(judged.pid, exited_owner_pid);

        // A replacement between the judgment and deletion must survive. This
        // turns compare-and-delete into an observed contract rather than a
        // timing-dependent expectation.
        write_synthetic_manifest_at_path_for_test(
            &manifest_path,
            &owner,
            "competing-scope",
            std::process::id(),
            0,
            None,
        );
        assert!(!reclaim_manifest_if_unchanged(&manifest_path, &judged)
            .expect("reject changed owner manifest"));
        let competing = read_manifest(&manifest_path).expect("competing owner manifest remains");
        assert_eq!(competing.project_scope_key, "competing-scope");

        write_synthetic_manifest_at_path_for_test(
            &manifest_path,
            &owner,
            "owner-scope",
            exited_owner_pid,
            0,
            None,
        );
        let unchanged = read_manifest(&manifest_path).expect("unchanged exited owner manifest");
        assert_eq!(unchanged.pid, exited_owner_pid);

        // Claiming from the sibling performs the production liveness probe,
        // compare-and-delete reclaim, and replacement as one lifecycle.
        let claim = claim_or_open_read_only(
            Some(&configured_storage),
            &sibling,
            key,
            "sibling-scope",
            false,
            None,
        )
        .expect("replace reclaimed owner manifest");
        let replacement = read_manifest(&manifest_path).expect("replacement from resolved storage");

        assert_eq!(claim.status.mode, ArtifactOwnerMode::Owner);
        assert_eq!(claim.status.owner_project_scope_key, "sibling-scope");
        assert!(claim.lease.is_some());
        assert_eq!(replacement.project_scope_key, "sibling-scope");
        assert_eq!(replacement.checkout_path, sibling.display().to_string());
    }

    #[test]
    fn linked_worktree_common_dir_routes_to_read_only_without_replacing_owner() {
        let _env_lock = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("owner");
        let linked = temp.path().join("linked");
        let common = temp.path().join("common.git");
        fs::create_dir_all(&owner).unwrap();
        fs::create_dir_all(&linked).unwrap();
        fs::create_dir_all(&common).unwrap();
        let key = "shared-key";

        claim_or_open_read_only(
            Some(temp.path()),
            &owner,
            key,
            "owner-scope",
            false,
            Some(&common),
        )
        .unwrap();
        let owner_manifest =
            read_manifest(&temp.path().join("artifact-owners/shared-key/owner.json"))
                .expect("owner manifest");
        let claim = claim_or_open_read_only(
            Some(temp.path()),
            &linked,
            key,
            "linked-scope",
            true,
            Some(&common),
        )
        .unwrap();
        let manifest_after =
            read_manifest(&temp.path().join("artifact-owners/shared-key/owner.json"))
                .expect("owner manifest after linked route");

        assert_eq!(claim.status.mode, ArtifactOwnerMode::ReadOnly);
        assert!(claim.lease.is_none());
        assert_eq!(
            manifest_after.project_scope_key,
            owner_manifest.project_scope_key
        );
        assert_eq!(manifest_after.checkout_path, owner_manifest.checkout_path);
    }

    #[test]
    fn heartbeat_advances_while_mutating_lane_is_busy() {
        let _serial = heartbeat_serial_guard();
        let _env_lock = crate::test_env::process_env_lock();
        let _interval = set_test_heartbeat_interval(25);
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let (status, lease) = claim_stale_owner(temp.path(), &root);
        let manifest_path = lease.path.clone();
        let ctx = Arc::new(context_with_artifact_owner(status, lease));
        let root_id = ProjectRootId::from_path(&root).unwrap();
        let executor = Executor::new();
        executor.register_actor(root_id.clone(), Arc::clone(&ctx));

        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let hold = executor.submit(
            root_id,
            Lane::Mutating,
            "hold-mutating-lane".to_string(),
            Box::new(move |_| {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Response::success("hold-mutating-lane", json!({ "released": true }))
            }),
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("mutating lane job started");

        let manifest = wait_for_heartbeat(&manifest_path, 0);
        assert!(manifest.heartbeat_at_ms > 0);

        release_tx.send(()).unwrap();
        hold.recv_timeout(Duration::from_secs(1))
            .expect("held lane released");
    }

    #[test]
    fn lease_release_stops_heartbeat() {
        let _serial = heartbeat_serial_guard();
        let _env_lock = crate::test_env::process_env_lock();
        let _interval = set_test_heartbeat_interval(25);
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let (status, lease) = claim_stale_owner(temp.path(), &root);
        let manifest_path = lease.path.clone();
        let ctx = context_with_artifact_owner(status, lease);

        let _manifest = wait_for_heartbeat(&manifest_path, 0);
        ctx.set_artifact_owner(None, None);
        assert_heartbeat_stops(&manifest_path);
    }

    #[test]
    fn context_shutdown_releases_heartbeat() {
        let _serial = heartbeat_serial_guard();
        let _env_lock = crate::test_env::process_env_lock();
        let _interval = set_test_heartbeat_interval(25);
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let (status, lease) = claim_stale_owner(temp.path(), &root);
        let manifest_path = lease.path.clone();
        let ctx = context_with_artifact_owner(status, lease);

        let _manifest = wait_for_heartbeat(&manifest_path, 0);
        drop(ctx);
        assert_heartbeat_stops(&manifest_path);
    }
}
