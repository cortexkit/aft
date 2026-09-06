use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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
                    return write_owner_manifest(
                        &path,
                        project_key,
                        project_scope_key,
                        &checkout_path,
                        git_common_dir.as_deref(),
                    );
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
                match create_owner_manifest(
                    &path,
                    project_key,
                    project_scope_key,
                    &checkout_path,
                    git_common_dir.as_deref(),
                ) {
                    Ok(claim) => return Ok(claim),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
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
        self.manifest.heartbeat_at_ms = now;
        atomic_write_manifest(&self.path, &self.manifest)?;
        self.last_heartbeat_ms = now;
        Ok(true)
    }
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
    write_manifest_to_file(&mut file, &manifest)?;
    file.sync_all()?;
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

fn read_manifest(path: &Path) -> Result<ArtifactOwnerManifest, ReadManifestError> {
    let bytes = fs::read(path).map_err(|error| {
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
        write_manifest_to_file(&mut file, manifest)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_parent(path);
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn write_manifest_to_file(file: &mut File, manifest: &ArtifactOwnerManifest) -> io::Result<()> {
    serde_json::to_writer(&mut *file, manifest).map_err(io::Error::other)?;
    file.write_all(b"\n")
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
    crate::bash_background::storage_dir(storage_dir)
        .join("artifact-owners")
        .join(project_key)
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
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
