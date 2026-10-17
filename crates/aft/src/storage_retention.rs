//! Durable checkout retention, independent of the roots an actor has opened.
//!
//! A missing path is necessary but not sufficient for deletion: binding history
//! must also be old, and all writers, pins, readers and local SQLite handles must
//! have gone. Unknown keys receive a fresh observation grace instead of treating
//! an old payload mtime as proof that a root has not recently been bound.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, ReadDir};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicU64, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::context::SubcLifecycleAdmission;

pub(crate) const ROOT_GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// A deleted linked worktree (a worker checkout) has its caches removed on
/// the first pass once this long has passed since its last bind. It is not
/// zero so that a session that bound the checkout seconds before deletion
/// can still finish, and a checkout recreated at the same path rebinds before
/// its caches can be removed. With the 10-minute pass interval and the
/// due-first lane, the caches go on the first pass after deletion. Every
/// other check still applies: the root must be confirmed missing, its volume
/// must match, and nothing may hold a writer, pin, reader or SQLite handle.
const LINKED_WORKTREE_REBIND_MARGIN_MS: u64 = 10 * 60 * 1000;
/// A checkout view folder (`views/<scope>`) that no family registry lists and
/// no binding history describes was never tied to a checkout this storage
/// knows about, typically an abandoned build. It is reclaimed once it has
/// been observed for this long and none of its files changed for this long.
const ORPHAN_VIEW_GRACE_MS: u64 = 60 * 60 * 1000;
/// Caches of checkouts already past their grace that one pass processes
/// before resuming the lexical walk.
const DUE_FIRST_ENTRIES: usize = 256;
const INTERVAL: Duration = Duration::from_secs(10 * 60);
// Startup can warm many checkout views at once. Keep storage-wide V1 blob
// marking (which must exclude pin admission) off that request-critical lane.
const STARTUP_GRACE: Duration = Duration::from_secs(5 * 60);
const PASS_BUDGET: Duration = Duration::from_secs(5);
const PASS_ENTRIES: usize = 64;
const TREE_ENTRIES: usize = 4096;
const HISTORY_ENTRIES: usize = 8192;
const DOMAINS: [&str; 6] = [
    "views",
    "callgraph",
    "inspect",
    "index",
    "semantic",
    "symbols",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Binding {
    pub root: PathBuf,
    pub artifact_key: String,
    pub last_bound_ms: u64,
    #[serde(default)]
    volume: Option<(PathBuf, u64)>,
    #[serde(default)]
    stable_volume: Option<VolumeIdentity>,
    #[serde(default)]
    linked_worktree: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct SweepReport {
    pub at_ms: u64,
    pub examined: usize,
    pub removed_roots: usize,
    pub removed_generations: usize,
    pub removed_bytes: u64,
    pub examined_blobs: usize,
    pub removed_blobs: usize,
    pub removed_blob_payload_bytes: u64,
    pub blob_reclaim_deferred: bool,
    pub examined_bindings: usize,
    pub pruned_bindings: usize,
    pub skipped_live: usize,
    pub skipped_recent: usize,
    pub skipped_mount_mismatch: usize,
    pub skipped_protected: usize,
    /// Caches kept because a fact needed to judge them could not be read.
    pub skipped_unknown: usize,
    pub unmeasured_removed_roots: usize,
    /// Binding records written before linked-worktree topology was recorded,
    /// upgraded in place while their checkout still existed.
    pub upgraded_bindings: usize,
    /// Keys whose every binding is past its grace with caches still on disk
    /// when the pass began.
    pub due_keys: usize,
    /// Vanished-checkout view folders still due after this pass. Protected
    /// folders remain in this count so a stalled backlog is visible in health.
    pub due_views_remaining: usize,
    /// The share of this pass spent on keys already past their grace.
    pub due_first: DomainReport,
    /// The share of this pass spent on view folders with no registry member
    /// and no binding history.
    pub orphan_views: DomainReport,
    pub stopped_early: bool,
    pub cancelled: bool,
    pub errors: Vec<String>,
    pub domains: BTreeMap<String, DomainReport>,
    /// Wall time by stage, including setup and blob work outside the root budget.
    pub timings_ms: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct DomainReport {
    /// Root directory entries (or blob rows for the blobs domains) evaluated.
    pub examined: usize,
    pub skipped_recent: usize,
    pub skipped_live: usize,
    pub skipped_mount_mismatch: usize,
    pub skipped_protected: usize,
    pub skipped_unknown: usize,
    pub removed_roots: usize,
    pub removed_bytes: u64,
    pub unmeasured_removed_roots: usize,
}

impl DomainReport {
    fn from_report(report: &SweepReport) -> Self {
        Self {
            examined: report.examined,
            skipped_recent: report.skipped_recent,
            skipped_live: report.skipped_live,
            skipped_mount_mismatch: report.skipped_mount_mismatch,
            skipped_protected: report.skipped_protected,
            skipped_unknown: report.skipped_unknown,
            removed_roots: report.removed_roots,
            removed_bytes: report.removed_bytes,
            unmeasured_removed_roots: report.unmeasured_removed_roots,
        }
    }

    fn difference(&self, before: &Self) -> Self {
        Self {
            examined: self.examined - before.examined,
            skipped_recent: self.skipped_recent - before.skipped_recent,
            skipped_live: self.skipped_live - before.skipped_live,
            skipped_mount_mismatch: self.skipped_mount_mismatch - before.skipped_mount_mismatch,
            skipped_protected: self.skipped_protected - before.skipped_protected,
            skipped_unknown: self.skipped_unknown - before.skipped_unknown,
            removed_roots: self.removed_roots - before.removed_roots,
            removed_bytes: self.removed_bytes - before.removed_bytes,
            unmeasured_removed_roots: self.unmeasured_removed_roots
                - before.unmeasured_removed_roots,
        }
    }

    fn add(&mut self, other: &Self) {
        self.examined += other.examined;
        self.skipped_recent += other.skipped_recent;
        self.skipped_live += other.skipped_live;
        self.skipped_mount_mismatch += other.skipped_mount_mismatch;
        self.skipped_protected += other.skipped_protected;
        self.skipped_unknown += other.skipped_unknown;
        self.removed_roots += other.removed_roots;
        self.removed_bytes += other.removed_bytes;
        self.unmeasured_removed_roots += other.unmeasured_removed_roots;
    }
}

#[derive(Default)]
struct State {
    startup_observed_at: Option<Instant>,
    last_run: Option<Instant>,
    running: bool,
    report: Option<SweepReport>,
    /// The last key the due-first lane processed, so keys that stay protected
    /// cannot starve the ones after them.
    due_cursor: Option<String>,
    reaping: bool,
}

pub(crate) fn startup_sweeps_ready(storage: &Path) -> bool {
    startup_sweeps_ready_at(storage, Instant::now())
}

fn startup_sweeps_ready_at(storage: &Path, now: Instant) -> bool {
    let mut states = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states.entry(storage.to_path_buf()).or_default();
    let started = *state.startup_observed_at.get_or_insert(now);
    now.saturating_duration_since(started) >= STARTUP_GRACE
}

fn states() -> &'static Mutex<HashMap<PathBuf, State>> {
    static STATES: OnceLock<Mutex<HashMap<PathBuf, State>>> = OnceLock::new();
    STATES.get_or_init(Default::default)
}

pub(crate) fn snapshot(storage: &Path) -> Option<SweepReport> {
    states().lock().ok()?.get(storage)?.report.clone()
}

#[cfg(test)]
pub(crate) type TestHook = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
pub(crate) struct TestHookGuard {
    path: PathBuf,
    previous: Option<TestHook>,
}

#[cfg(test)]
impl Drop for TestHookGuard {
    fn drop(&mut self) {
        let mut hooks = test_hooks().lock().unwrap();
        if let Some(previous) = self.previous.take() {
            hooks.insert(self.path.clone(), previous);
        } else {
            hooks.remove(&self.path);
        }
    }
}

#[cfg(test)]
pub(crate) fn observe_test_hook(path: &Path, hook: TestHook) -> TestHookGuard {
    let path = path.to_path_buf();
    let previous = test_hooks().lock().unwrap().insert(path.clone(), hook);
    TestHookGuard { path, previous }
}

#[cfg(test)]
fn test_hooks() -> &'static Mutex<HashMap<PathBuf, TestHook>> {
    static HOOKS: OnceLock<Mutex<HashMap<PathBuf, TestHook>>> = OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn test_hook(path: &Path, step: &str) {
    let hook = test_hooks().lock().unwrap().get(path).cloned();
    if let Some(hook) = hook {
        hook(step);
    }
}

#[cfg(test)]
pub(crate) fn allow_next_scheduled_pass_for_test(storage: &Path) {
    let mut states = states().lock().unwrap();
    let state = states.entry(storage.to_path_buf()).or_default();
    state.startup_observed_at = Some(Instant::now() - STARTUP_GRACE);
    assert!(
        !state.running,
        "previous scheduled pass must have completed"
    );
    state.last_run = None;
}

#[cfg(test)]
pub(crate) fn scheduled_pass_finished_for_test(storage: &Path) -> bool {
    states()
        .lock()
        .unwrap()
        .get(storage)
        .is_some_and(|state| !state.running && state.report.is_some())
}

fn key_valid(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn barrier(storage: &Path) -> io::Result<crate::fs_lock::LockGuard> {
    fs::create_dir_all(storage.join("retention"))?;
    crate::fs_lock::try_acquire(
        &storage.join("retention/sweep.lock"),
        Duration::from_secs(5),
    )
    .map_err(|error| io::Error::other(error.to_string()))
}

/// V1 has no ref-epoch handoff. Serialize pin admission with its marking pass.
/// V2 pins use the family registry's epoch/barrier protocol instead.
pub(crate) fn pin_barrier(view_dir: &Path) -> io::Result<Option<crate::fs_lock::LockGuard>> {
    let Some(views) = view_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "views"))
    else {
        return Ok(None);
    };
    views.parent().map(barrier).transpose()
}

fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    crate::backup::create_private_durable_dir(
        path.parent()
            .ok_or_else(|| io::Error::other("missing retention parent"))?,
    )?;
    crate::durability::atomic_write(
        path,
        &serde_json::to_vec(value).map_err(io::Error::other)?,
        crate::durability::AtomicWriteDurability::FileAndDirectory,
    )
}

/// Called before cache acquisition, on every configure, even a same-key rebind.
pub(crate) fn record_bind(storage: &Path, root: &Path, artifact_key: &str) -> io::Result<()> {
    let _barrier = barrier(storage)?;
    let root = fs::canonicalize(root)?;
    let scope = crate::path_identity::project_scope_key(&root);
    let binding = Binding {
        volume: volume_anchor(&root),
        stable_volume: Some(VolumeIdentity {
            uuid: volume_uuid(&root),
            boot_id: boot_id(),
        }),
        linked_worktree: linked_worktree_at_bind(&root),
        root,
        artifact_key: artifact_key.to_owned(),
        last_bound_ms: crate::pins::now_ms(),
    };
    atomic_json(
        &storage
            .join("retention/roots")
            .join(format!("{scope}.json")),
        &binding,
    )?;
    clear_unknown_observation(storage, artifact_key)
}

/// Only a verified linked-worktree Git layout gets the shorter retention age.
/// A Git file alone can also describe a submodule or a separate main Git dir.
fn linked_worktree_at_bind(root: &Path) -> bool {
    let marker = root.join(".git");
    if !fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return false;
    }
    let Some(git_dir) = fs::read_to_string(&marker).ok().and_then(|text| {
        let path = text.strip_prefix("gitdir:")?.trim();
        (!path.is_empty())
            .then(|| fs::canonicalize(root.join(path)).ok())
            .flatten()
    }) else {
        return false;
    };
    let Some(common) = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .and_then(|text| fs::canonicalize(git_dir.join(text.trim())).ok())
    else {
        return false;
    };
    let backlink = fs::read_to_string(git_dir.join("gitdir"))
        .ok()
        .and_then(|text| fs::canonicalize(git_dir.join(text.trim())).ok());
    git_dir.is_dir()
        && !common.starts_with(root)
        && git_dir.parent() == Some(common.join("worktrees").as_path())
        && backlink.as_deref() == Some(marker.as_path())
}

#[cfg(unix)]
fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|metadata| metadata.dev())
}
#[cfg(windows)]
fn device(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumePathNameW(path: *const u16, volume: *mut u16, length: u32) -> i32;
        fn GetVolumeInformationW(
            volume: *const u16,
            name: *mut u16,
            name_length: u32,
            serial: *mut u32,
            component_length: *mut u32,
            flags: *mut u32,
            filesystem: *mut u16,
            filesystem_length: u32,
        ) -> i32;
    }
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut volume = vec![0_u16; 32768];
    let mut serial = 0;
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) == 0 {
            return None;
        }
        if GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        ) == 0
        {
            return None;
        }
    }
    Some(u64::from(serial))
}
#[cfg(not(any(unix, windows)))]
fn device(_path: &Path) -> Option<u64> {
    None
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct VolumeIdentity {
    uuid: Option<String>,
    boot_id: Option<String>,
}

#[cfg(target_os = "macos")]
fn mount_point(path: &Path) -> Option<PathBuf> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // statfs returns the actual mount, including the APFS Data volume behind
    // firmlinks; walking lexical parents can instead end at the system snapshot.
    let info = unsafe {
        if libc::statfs(path.as_ptr(), info.as_mut_ptr()) != 0 {
            return None;
        }
        info.assume_init()
    };
    let bytes = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) }.to_bytes();
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(target_os = "macos")]
fn volume_uuid(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut attributes = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: 0,
        volattr: 0x8000_0000 | 0x0004_0000, // ATTR_VOL_INFO | ATTR_VOL_UUID
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist packs a four-byte length followed by the 16-byte volume UUID.
    let mut buffer = [0_u8; 20];
    let result = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        )
    };
    if result != 0
        || u32::from_ne_bytes(buffer[..4].try_into().ok()?) != 20
        || buffer[4..].iter().all(|b| *b == 0)
    {
        return None;
    }
    Some(buffer[4..].iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(target_os = "linux")]
fn volume_uuid(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = device(path)?;
    for entry in fs::read_dir("/dev/disk/by-uuid").ok()? {
        let entry = entry.ok()?;
        if fs::metadata(entry.path()).ok()?.rdev() == dev {
            return entry.file_name().to_str().map(str::to_owned);
        }
    }
    None
}

#[cfg(windows)]
fn volume_uuid(path: &Path) -> Option<String> {
    device(path).map(|serial| format!("windows-volume-serial:{serial}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn volume_uuid(_path: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn boot_id() -> Option<String> {
    let mut buffer = [0_u8; 128];
    let mut size = buffer.len();
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size > buffer.len() {
        return None;
    }
    std::str::from_utf8(&buffer[..size])
        .ok()
        .map(|s| s.trim_end_matches('\0').to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn boot_id() -> Option<String> {
    None
}

pub(crate) fn volume_anchor(root: &Path) -> Option<(PathBuf, u64)> {
    #[cfg(target_os = "macos")]
    {
        let anchor = mount_point(root)?;
        return Some((anchor.clone(), device(&anchor)?));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let dev = device(root)?;
        let mut anchor = root;
        while let Some(parent) = anchor.parent() {
            if device(parent) != Some(dev) {
                break;
            }
            anchor = parent;
        }
        Some((anchor.to_path_buf(), dev))
    }
}

fn mount_matches(binding: &Binding) -> bool {
    #[cfg(target_os = "macos")]
    let boot_volume = mount_point(Path::new("/")).as_deref() == Some(Path::new("/"));
    #[cfg(not(target_os = "macos"))]
    let boot_volume = false;
    #[cfg(test)]
    let uuid_at = |path: &Path| {
        let probe_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(uuid) = test_volume_uuids()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&probe_path)
        {
            return Some(uuid.clone());
        }
        volume_uuid(path)
    };
    #[cfg(not(test))]
    let uuid_at = volume_uuid;
    mount_matches_with(
        binding,
        &uuid_at,
        &device,
        boot_id().as_deref(),
        boot_volume,
    )
}

#[cfg(test)]
fn test_volume_uuids() -> &'static Mutex<HashMap<PathBuf, String>> {
    static UUIDS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    UUIDS.get_or_init(Default::default)
}

fn mount_matches_with(
    binding: &Binding,
    uuid_at: &dyn Fn(&Path) -> Option<String>,
    device_at: &dyn Fn(&Path) -> Option<u64>,
    current_boot: Option<&str>,
    boot_volume: bool,
) -> bool {
    let Some((anchor, dev)) = &binding.volume else {
        // Memo/owner bindings without volume information are accepted only for
        // managed worker paths, never for an unattributed external checkout.
        return binding
            .root
            .to_string_lossy()
            .contains("/cortexkit/alfonso/worktrees/");
    };
    if let Some(identity) = &binding.stable_volume {
        if let Some(uuid) = &identity.uuid {
            return uuid_at(anchor).as_ref() == Some(uuid)
                || binding
                    .root
                    .parent()
                    .is_some_and(|parent| uuid_at(parent).as_ref() == Some(uuid));
        }
        // Without a UUID, a device number is useful only within the recorded boot.
        return identity.boot_id.is_some()
            && identity.boot_id.as_deref() == current_boot
            && device_at(anchor) == Some(*dev);
    }
    if boot_volume && anchor == Path::new("/") {
        // Legacy boot-volume bindings cannot represent an unmounted external
        // disk. The running system snapshot may receive a new st_dev at reboot.
        return true;
    }
    device_at(anchor) == Some(*dev)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootEligibility {
    Due,
    Live,
    Recent,
    MountMismatch,
}

fn binding_eligibility(binding: &Binding, now: u64) -> RootEligibility {
    binding_eligibility_with_mount(binding, now, mount_matches(binding))
}

fn binding_eligibility_with_mount(binding: &Binding, now: u64, mounted: bool) -> RootEligibility {
    if !matches!(binding.root.try_exists(), Ok(false)) {
        return RootEligibility::Live;
    }
    if !mounted {
        return RootEligibility::MountMismatch;
    }
    let grace = if binding.linked_worktree {
        LINKED_WORKTREE_REBIND_MARGIN_MS
    } else {
        ROOT_GRACE_MS
    };
    if now.saturating_sub(binding.last_bound_ms) < grace {
        RootEligibility::Recent
    } else {
        RootEligibility::Due
    }
}

fn missing_and_old(binding: &Binding, now: u64) -> bool {
    binding_eligibility(binding, now) == RootEligibility::Due
}

pub(crate) fn missing_root_due(storage: &Path, root: &Path, last_bind_ms: u64) -> bool {
    missing_root_eligibility(storage, root, last_bind_ms) == RootEligibility::Due
}

pub(crate) fn missing_root_eligibility(
    storage: &Path,
    root: &Path,
    last_bind_ms: u64,
) -> RootEligibility {
    let root = crate::root_cache::canonical_root(root);
    let scope = missing_root_scope_key(&root);
    let mut binding = read_json::<Binding>(&storage.join(format!("retention/roots/{scope}.json")))
        .unwrap_or_else(|_| Binding {
            root,
            artifact_key: String::new(),
            last_bound_ms: last_bind_ms,
            volume: None,
            stable_volume: None,
            linked_worktree: false,
        });
    binding.last_bound_ms = binding.last_bound_ms.max(last_bind_ms);
    binding_eligibility(&binding, crate::pins::now_ms())
}

// A missing checkout still needs the identity spelling used when it was bound.
// In particular, Windows fs::canonicalize returns a verbatim (\\?\) path while
// ProjectRootId uses its normalized identity spelling. The lexical fallback for
// a now-missing verbatim path otherwise hashes a different key and misses the
// durable binding. Resolve an existing ancestor through the same identity API,
// then append the missing components without trying to canonicalize them.
fn missing_root_scope_key(root: &Path) -> String {
    let mut ancestor = root;
    let mut missing = Vec::new();
    loop {
        if let Ok(identity) = crate::path_identity::ProjectRootId::from_path(ancestor) {
            let mut normalized = identity.into_path_buf();
            for component in missing.iter().rev() {
                normalized.push(component);
            }
            return crate::path_identity::project_scope_key(&normalized);
        }
        let Some(component) = ancestor.file_name() else {
            return crate::path_identity::project_scope_key(root);
        };
        missing.push(component.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return crate::path_identity::project_scope_key(root);
        };
        ancestor = parent;
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    if fs::metadata(path)?.len() > 8 * 1024 * 1024 {
        return Err(io::Error::other("retention metadata exceeds 8 MiB"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

fn bounded_dirs(directory: &Path, cap: usize) -> io::Result<Vec<fs::DirEntry>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let result = entries.take(cap + 1).collect::<io::Result<Vec<_>>>()?;
    if result.len() > cap {
        return Err(io::Error::other(format!(
            "retention walk exceeds {cap} entries at {}",
            directory.display()
        )));
    }
    Ok(result)
}

fn bindings(storage: &Path) -> io::Result<BTreeMap<String, Vec<Binding>>> {
    #[derive(Deserialize)]
    struct Memo {
        key: String,
        recorded_at_ms: u64,
    }
    let mut result: BTreeMap<String, Vec<Binding>> = BTreeMap::new();
    #[cfg(test)]
    test_hook(storage, "history-read");
    let mut linked_roots = HashMap::new();
    let memo = storage.join("cache-keys.json");
    if memo.exists() {
        let memo: BTreeMap<String, Memo> = read_json(&memo)?;
        if memo.len() > HISTORY_ENTRIES {
            return Err(io::Error::other("retention history exceeds 8192 bindings"));
        }
        for (root, entry) in memo {
            let binding = Binding {
                root: root.into(),
                artifact_key: entry.key,
                last_bound_ms: entry.recorded_at_ms,
                volume: None,
                stable_volume: None,
                linked_worktree: false,
            };
            result
                .entry(missing_root_scope_key(&binding.root))
                .or_default()
                .push(binding.clone());
            result
                .entry(binding.artifact_key.clone())
                .or_default()
                .push(binding);
        }
    }
    for entry in bounded_dirs(&storage.join("retention/roots"), HISTORY_ENTRIES)? {
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let binding: Binding = read_json(&entry.path())?;
        if binding.linked_worktree {
            linked_roots.insert(
                crate::root_cache::canonical_root(&binding.root),
                (binding.volume.clone(), binding.stable_volume.clone()),
            );
        }
        let scope = entry
            .path()
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        // A durable binding supersedes the memo's possibly much older timestamp.
        result.entry(scope).or_default().push(binding.clone());
        result
            .entry(binding.artifact_key.clone())
            .or_default()
            .push(binding);
    }
    for entry in bounded_dirs(&storage.join("artifact-owners"), HISTORY_ENTRIES)? {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("owner.json");
        if !path.exists() {
            continue;
        }
        let owner: crate::artifact_owner::ArtifactOwnerManifest = read_json(&path)?;
        if owner.schema_version != crate::artifact_owner::SCHEMA_VERSION {
            return Err(io::Error::other("unknown artifact owner format"));
        }
        let binding = Binding {
            root: owner.checkout_path.into(),
            artifact_key: entry.file_name().to_string_lossy().into_owned(),
            last_bound_ms: owner.heartbeat_at_ms,
            volume: None,
            stable_volume: None,
            linked_worktree: false,
        };
        result
            .entry(owner.project_scope_key)
            .or_default()
            .push(binding.clone());
        result
            .entry(binding.artifact_key.clone())
            .or_default()
            .push(binding);
    }
    // Memo and owner records retain their own clocks and root protections.
    // When a durable bind verified linked topology, its mount identity also
    // identifies same-root legacy copies that did not record either fact.
    for records in result.values_mut() {
        for binding in records {
            if let Some((volume, stable_volume)) =
                linked_roots.get(&crate::root_cache::canonical_root(&binding.root))
            {
                binding.linked_worktree = true;
                if binding.volume.is_none() {
                    binding.volume = volume.clone();
                    binding.stable_volume = stable_volume.clone();
                }
            }
        }
    }
    Ok(result)
}

fn domain_paths(storage: &Path) -> io::Result<Vec<(PathBuf, bool)>> {
    let mut result = DOMAINS
        .iter()
        .map(|domain| (storage.join(domain), false))
        .collect::<Vec<_>>();
    for entry in bounded_dirs(storage, 128)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir()
            && (matches!(name.as_str(), "opencode" | "pi" | "runner")
                || name.starts_with("mcp--")
                || name.starts_with("fed--"))
        {
            for domain in ["callgraph", "inspect"] {
                result.push((entry.path().join(domain), true));
            }
        }
    }
    Ok(result)
}

/// A lexical entry name, rather than a ReadDir offset, survives process and
/// directory replacement. New names behind the cursor are visited on the next lap.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct TraversalCursor {
    domain: PathBuf,
    last_processed: Option<PathBuf>,
}

struct TraversalEntry {
    directory: PathBuf,
    flat: bool,
    entry: fs::DirEntry,
    after: TraversalCursor,
}

struct TraversalBatch {
    entries: Vec<TraversalEntry>,
    end: TraversalCursor,
    cursor_path: PathBuf,
}

/// Retain at most `limit` names, ordered lexically after the last processed name.
/// Directory enumeration is metadata-only; no cache payload or SQLite file is opened.
fn traversal_batch(
    storage: &Path,
    domains: &[(PathBuf, bool)],
    kind: &str,
    limit: usize,
) -> io::Result<TraversalBatch> {
    traversal_batch_filtered(storage, domains, kind, limit, &|_| true)
}

fn traversal_batch_filtered(
    storage: &Path,
    domains: &[(PathBuf, bool)],
    kind: &str,
    limit: usize,
    include: &dyn Fn(&fs::DirEntry) -> bool,
) -> io::Result<TraversalBatch> {
    let cursor_path = storage
        .join("retention/cursors")
        .join(format!("{kind}.json"));
    let mut cursor: TraversalCursor = match read_json(&cursor_path) {
        Ok(cursor) => cursor,
        Err(error) if error.kind() == io::ErrorKind::NotFound => TraversalCursor::default(),
        Err(error) => return Err(error),
    };
    let mut domain = domains
        .iter()
        .position(|(path, _)| path.strip_prefix(storage).ok() == Some(cursor.domain.as_path()))
        .unwrap_or(0);
    if domains.is_empty() {
        return Ok(TraversalBatch {
            entries: Vec::new(),
            end: cursor,
            cursor_path,
        });
    }
    let mut result = Vec::new();
    for _ in 0..domains.len() {
        let (directory, flat) = &domains[domain];
        let relative = directory
            .strip_prefix(storage)
            .map_err(io::Error::other)?
            .to_path_buf();
        if cursor.domain != relative {
            cursor = TraversalCursor {
                domain: relative.clone(),
                last_processed: None,
            };
        }
        let remaining = limit - result.len();
        let mut names = BTreeMap::new();
        match fs::read_dir(directory) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_name() == ".retention-trash" || !include(&entry) {
                        continue;
                    }
                    let name = PathBuf::from(entry.file_name());
                    if cursor
                        .last_processed
                        .as_ref()
                        .is_some_and(|last| name <= *last)
                    {
                        continue;
                    }
                    names.insert(name, entry);
                    if names.len() > remaining {
                        names.pop_last();
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for (name, entry) in names {
            cursor.last_processed = Some(name);
            result.push(TraversalEntry {
                directory: directory.clone(),
                flat: *flat,
                entry,
                after: cursor.clone(),
            });
        }
        if result.len() == limit {
            break;
        }
        domain = (domain + 1) % domains.len();
        cursor = TraversalCursor {
            domain: domains[domain]
                .0
                .strip_prefix(storage)
                .map_err(io::Error::other)?
                .to_path_buf(),
            last_processed: None,
        };
    }
    Ok(TraversalBatch {
        entries: result,
        end: cursor,
        cursor_path,
    })
}

fn next_batch(storage: &Path, domains: &[(PathBuf, bool)]) -> io::Result<TraversalBatch> {
    traversal_batch(storage, domains, "roots", PASS_ENTRIES)
}

fn visit_one_family(
    storage: &Path,
    domain: &str,
    kind: &str,
    cancelled: &dyn Fn() -> bool,
    mut work: impl FnMut(&fs::DirEntry) -> io::Result<()>,
) -> io::Result<()> {
    let batch = traversal_batch(storage, &[(storage.join(domain), false)], kind, 1)?;
    if cancelled() {
        return Ok(());
    }
    let Some(item) = batch.entries.into_iter().next() else {
        return atomic_json(&batch.cursor_path, &batch.end);
    };
    let result = work(&item.entry);
    if !cancelled() {
        // Advance even when family work fails, so later families can still run.
        atomic_json(&batch.cursor_path, &item.after)?;
    }
    result
}

fn protected(cache: &Path) -> io::Result<bool> {
    for entry in bounded_dirs(&cache.join("pins"), TREE_ENTRIES)? {
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let pin = crate::pins::read_metadata_strict(&entry.path())
                .map_err(|error| io::Error::other(error.to_string()))?;
            if crate::pins::owner_is_live(&pin.owner) {
                return Ok(true);
            }
        }
    }
    for entry in bounded_dirs(&cache.join("readers"), TREE_ENTRIES)? {
        if !entry.file_type()?.is_dir() {
            return Ok(true);
        }
        if crate::root_cache::protected_read_marker_exists(
            cache,
            &entry.file_name().to_string_lossy(),
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn owner_has_residency_marker(
    storage: &Path,
    owner: &crate::artifact_owner::ArtifactOwnerManifest,
) -> bool {
    let directory = storage
        .join("retention/readers")
        .join(&owner.project_scope_key);
    let Ok(entries) = bounded_dirs(&directory, TREE_ENTRIES) else {
        return false;
    };
    entries.iter().any(|entry| {
        read_json::<crate::root_cache::ReadMarkerMetadata>(&entry.path()).is_ok_and(|marker| {
            marker.pid == owner.pid
                && marker.hostname == owner.hostname
                && crate::root_cache::process_start_time_ms(marker.pid)
                    .is_some_and(|start| start <= marker.created_at_ms.saturating_add(1000))
        })
    })
}

/// Metadata only: never open a raw file descriptor on a SQLite file set.
fn tree_files(cache: &Path, deadline: Instant) -> io::Result<Vec<(PathBuf, u64)>> {
    validate_tree(cache, deadline, true)
}

/// A due root needs the filesystem safety checks, not payload sizes. Avoid a
/// stat of every file merely to report bytes that the background reaper frees.
fn validate_tree(
    cache: &Path,
    deadline: Instant,
    measure: bool,
) -> io::Result<Vec<(PathBuf, u64)>> {
    #[cfg(test)]
    test_hook(cache, "walk");
    if !fs::symlink_metadata(cache)?.is_dir() {
        return Err(io::Error::other("non-directory cache retained"));
    }
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(cache)?;
    let mut pending = vec![cache.to_path_buf()];
    let mut result = Vec::new();
    let mut examined = 0;
    while let Some(directory) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(io::Error::other("retention deadline reached"));
        }
        for entry in bounded_dirs(&directory, TREE_ENTRIES.saturating_sub(examined))? {
            if Instant::now() >= deadline {
                return Err(io::Error::other("retention deadline reached"));
            }
            examined += 1;
            let kind = entry.file_type()?;
            if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
                return Err(io::Error::other("non-regular cache entry retained"));
            }
            if kind.is_dir() {
                if !boundary.should_descend(&entry.path())? {
                    return Err(io::Error::other("foreign cache filesystem retained"));
                }
                pending.push(entry.path());
            } else if measure {
                #[cfg(test)]
                test_hook(cache, "measure-file");
                result.push((entry.path(), entry.metadata()?.len()));
            }
        }
    }
    Ok(result)
}

/// Keep trash on the cache's filesystem so retiring a root is a single rename.
/// A failed unlink or daemon exit leaves a recognizable directory for retry.
fn retire_directory(cache: &Path) -> io::Result<()> {
    let parent = cache
        .parent()
        .ok_or_else(|| io::Error::other("missing cache parent"))?;
    let trash = parent.join(".retention-trash");
    fs::create_dir_all(&trash)?;
    if !fs::symlink_metadata(&trash)?.is_dir() {
        return Err(io::Error::other("non-directory retention trash retained"));
    }
    #[cfg(test)]
    test_hook(cache, "rename-root");
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    fs::rename(
        cache,
        trash.join(format!("retired-{:032x}", u128::from_le_bytes(nonce))),
    )
}

fn reap_retired(storage: &Path) -> io::Result<()> {
    // Different daemon processes may discover the same tombstones. This lock
    // is separate from admission: slow unlinks must not block binds or pins.
    let _lease = crate::fs_lock::try_acquire(&storage.join("retention/reap.lock"), Duration::ZERO)
        .map_err(|error| io::Error::other(error.to_string()))?;
    for (domain, flat) in domain_paths(storage)? {
        if flat {
            continue;
        }
        let trash = domain.join(".retention-trash");
        let entries = match fs::read_dir(&trash) {
            Ok(entries) if fs::symlink_metadata(&trash)?.is_dir() => entries,
            Ok(_) => return Err(io::Error::other("non-directory retention trash retained")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.strip_prefix("retired-").is_some_and(|nonce| {
                nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                continue;
            }
            let path = entry.path();
            // Check entry types and filesystem boundaries again after a restart;
            // an unexpected symlink or mount must keep the trash, not be unlinked.
            if let Err(error) =
                validate_tree(&path, Instant::now() + Duration::from_secs(60), false)
            {
                crate::slog_warn!(
                    "storage retention trash={} retained: {error}",
                    path.display()
                );
                continue;
            }
            #[cfg(test)]
            test_hook(storage, "reap-root");
            if let Err(error) = fs::remove_dir_all(&path) {
                crate::slog_warn!(
                    "storage retention trash={} unlink deferred: {error}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn schedule_reaper(storage: &Path) {
    {
        let mut states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(storage.to_path_buf()).or_default();
        if state.reaping {
            return;
        }
        state.reaping = true;
    }
    let storage = storage.to_path_buf();
    let worker_storage = storage.clone();
    let spawned = std::thread::Builder::new()
        .name("aft-retention-reaper".into())
        .spawn(move || {
            struct Reset(PathBuf);
            impl Drop for Reset {
                fn drop(&mut self) {
                    if let Some(state) = states()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(&self.0)
                    {
                        state.reaping = false;
                    }
                }
            }
            let _reset = Reset(worker_storage.clone());
            if let Err(error) = reap_retired(&worker_storage) {
                crate::slog_warn!(
                    "storage retention reaper root={} error={error}",
                    worker_storage.display()
                );
            }
        });
    if let Err(error) = spawned {
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(storage.clone())
            .or_default()
            .reaping = false;
        crate::slog_warn!(
            "storage retention reaper spawn root={} error={error}",
            storage.display()
        );
    }
}

fn root_resident(storage: &Path, key: &str, history: &BTreeMap<String, Vec<Binding>>) -> bool {
    crate::root_cache::protected_read_marker_exists(&storage.join("retention"), key)
        || history.get(key).is_some_and(|records| {
            records.iter().any(|binding| {
                crate::root_cache::protected_read_marker_exists(
                    &storage.join("retention"),
                    &crate::path_identity::project_scope_key(&binding.root),
                )
            })
        })
}

/// The artifact-owner manifests under a storage folder, read once and then
/// consulted for many keys. Each manifest keeps its own read result, so a
/// key is refused on an unreadable manifest exactly when a fresh scan in the
/// same order would have hit it before finding a protecting owner.
struct OwnerManifests(
    io::Result<
        Vec<(
            std::ffi::OsString,
            io::Result<crate::artifact_owner::ArtifactOwnerManifest>,
        )>,
    >,
);

impl OwnerManifests {
    fn read(storage: &Path) -> Self {
        #[cfg(test)]
        test_hook(storage, "owner-scan");
        Self(
            bounded_dirs(&storage.join("artifact-owners"), HISTORY_ENTRIES).map(|owners| {
                owners
                    .into_iter()
                    .filter_map(|owner| {
                        let path = owner.path().join("owner.json");
                        path.exists().then(|| (owner.file_name(), read_json(&path)))
                    })
                    .collect()
            }),
        )
    }

    /// Whether a live owner protects `key`. `known` says whether the key has
    /// binding history; without it, any protected owner lacking a residency
    /// marker protects the key as well.
    fn protect(&self, storage: &Path, key: &str, known: bool) -> io::Result<bool> {
        let owners = self
            .0
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
        for (name, metadata) in owners {
            let metadata = metadata
                .as_ref()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
            if ((!known && !owner_has_residency_marker(storage, metadata))
                || name == key
                || metadata.project_scope_key == key)
                && crate::artifact_owner::protected_for_retention(metadata)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Most due caches one admission recheck covers. Prepared caches keep their
/// leases until that recheck, so this also bounds how many caches a pass
/// holds locked at once.
const ADMISSION_BATCH: usize = 16;

/// Due caches that passed every check that runs without the retention
/// barrier, waiting with their leases held for one shared admission recheck.
///
/// The recheck needs the binding history read under the barrier, and that
/// history is every binding record and owner manifest in the storage. Reading
/// it once per batch, instead of once per cache, keeps a backlog drain from
/// rereading thousands of records for every cache it removes. The barrier is
/// still taken only for the recheck and rename, never across the walks.
#[derive(Default)]
struct DueBatch {
    items: Vec<PreparedDue>,
    /// Owner manifests for the checks made before the retention barrier
    /// (`retention/sweep.lock`, which binds and pins also take), read once
    /// per pass. The admission recheck reads them again while holding the
    /// barrier, just before each rename.
    owners: Option<OwnerManifests>,
    /// Set while the due-first lane prepares caches, so their outcomes also
    /// count toward [`SweepReport::due_first`].
    due_lane: bool,
}

struct PreparedDue {
    directory: PathBuf,
    key: String,
    leases: Vec<crate::fs_lock::LockGuard>,
    due_lane: bool,
    deadline: Instant,
}

impl DueBatch {
    fn full(&self) -> bool {
        self.items.len() >= ADMISSION_BATCH
    }

    /// Recheck and retire every prepared cache under one barrier hold. Each
    /// cache's outcome is counted for its own domain, as it would have been
    /// had it been retired when its lane met it.
    fn flush(
        &mut self,
        storage: &Path,
        now: u64,
        cancelled: &dyn Fn() -> bool,
        report: &mut SweepReport,
    ) -> io::Result<()> {
        if self.items.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(&mut self.items);
        let phase = Instant::now();
        // Try the retention barrier without waiting. If a bind or pin holds it,
        // that caller is protecting this storage right now, so retention backs
        // off instead of spending its pass budget waiting.
        let admission =
            crate::fs_lock::try_acquire(&storage.join("retention/sweep.lock"), Duration::ZERO)
                .map_err(|error| io::Error::other(error.to_string()))
                .and_then(|barrier| {
                    Ok((barrier, bindings(storage)?, OwnerManifests::read(storage)))
                });
        *report
            .timings_ms
            .entry("due_admission_history".into())
            .or_default() += phase.elapsed().as_millis() as u64;
        let (barrier, current, owners) = match admission {
            Ok(admission) => admission,
            Err(error) => {
                for item in items {
                    let before = DomainReport::from_report(report);
                    item.refuse(&error, report);
                    item.attribute(storage, &before, report)?;
                }
                return Ok(());
            }
        };
        for item in items {
            let before = DomainReport::from_report(report);
            let cache = item.directory.join(&item.key);
            let directory = item.directory.clone();
            let due_lane = item.due_lane;
            if !report.cancelled {
                if let Err(error) = item.retire(storage, &current, &owners, now, cancelled, report)
                {
                    report.skipped_protected += 1;
                    if report.errors.len() < 16 {
                        report.errors.push(format!("{}: {error}", cache.display()));
                    }
                }
            }
            attribute_due(storage, &directory, due_lane, &before, report)?;
        }
        drop(barrier);
        Ok(())
    }
}

fn attribute_due(
    storage: &Path,
    directory: &Path,
    due_lane: bool,
    before: &DomainReport,
    report: &mut SweepReport,
) -> io::Result<()> {
    let delta = DomainReport::from_report(report).difference(before);
    if due_lane {
        report.due_first.add(&delta);
    }
    report
        .domains
        .entry(
            directory
                .strip_prefix(storage)
                .map_err(io::Error::other)?
                .to_string_lossy()
                .into_owned(),
        )
        .or_default()
        .add(&delta);
    Ok(())
}

impl PreparedDue {
    fn attribute(
        &self,
        storage: &Path,
        before: &DomainReport,
        report: &mut SweepReport,
    ) -> io::Result<()> {
        attribute_due(storage, &self.directory, self.due_lane, before, report)
    }

    /// The retention barrier (`retention/sweep.lock`) could not be taken: the
    /// cache stays, counted as `skipped_protected` with the error and the
    /// cache path, as a lane reports any candidate whose checks failed.
    fn refuse(&self, error: &io::Error, report: &mut SweepReport) {
        report.skipped_protected += 1;
        if report.errors.len() < 16 {
            report.errors.push(format!(
                "{}: {error}",
                self.directory.join(&self.key).display()
            ));
        }
    }

    /// Recheck admission against history read under the barrier, then retire
    /// the cache with a rename.
    fn retire(
        self,
        storage: &Path,
        current: &BTreeMap<String, Vec<Binding>>,
        owners: &OwnerManifests,
        now: u64,
        cancelled: &dyn Fn() -> bool,
        report: &mut SweepReport,
    ) -> io::Result<()> {
        let Self {
            directory,
            key,
            leases,
            deadline,
            ..
        } = self;
        let cache = directory.join(&key);
        let key = key.as_str();
        if !eligible(storage, key, current, now, ROOT_GRACE_MS, report)?
            || root_resident(storage, key, current)
            || crate::root_cache::live_scope_keys_for_storage(storage).contains(key)
            || owners.protect(storage, key, current.contains_key(key))?
            || protected(&cache)?
        {
            report.skipped_protected += 1;
            return Ok(());
        }
        let _files = crate::db::file_identity::filesystem_guard();
        if crate::db::file_identity::has_open_connections_under(&cache) {
            report.skipped_protected += 1;
            return Ok(());
        }
        if cancelled() {
            report.cancelled = true;
            return Ok(());
        }
        let phase = Instant::now();
        retire_directory(&cache)?;
        *report.timings_ms.entry("due_rename".into()).or_default() +=
            phase.elapsed().as_millis() as u64;
        report.removed_roots += 1;
        report.unmeasured_removed_roots += 1;
        if directory == storage.join("views") {
            report.due_views_remaining = report.due_views_remaining.saturating_sub(1);
        }
        if !cache_key_present(storage, key, deadline)? {
            clear_unknown_observation(storage, key)?;
        }
        // Lease guards address the old pathname. Drop them before releasing
        // admission so they cannot touch a newly bound replacement at that path.
        drop(leases);
        Ok(())
    }
}

/// A cache whose every binding is past its grace is about to be retired whole,
/// so it skips the obsolete-generation GC and the byte-measuring tree walks
/// that other candidates get. Validate its tree once without measuring, then
/// queue it in `batch` for the admission recheck that retires it.
fn prepare_due_cache(
    storage: &Path,
    directory: &Path,
    key: &str,
    history: &BTreeMap<String, Vec<Binding>>,
    deadline: Instant,
    batch: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<()> {
    let cache = directory.join(key);
    let mut leases = Vec::new();
    let local_writer = crate::root_cache::local_callgraph_writer(&cache);
    for name in [
        "writer.lease",
        "cache.lock",
        "symbols.lock",
        "semantic.lock",
    ] {
        if name == "writer.lease"
            && directory
                .file_name()
                .is_some_and(|name| name == "callgraph")
            && local_writer.is_some()
        {
            continue;
        }
        match crate::fs_lock::try_acquire(&cache.join(name), Duration::ZERO) {
            Ok(lease) => leases.push(lease),
            Err(_) => {
                report.skipped_protected += 1;
                return Ok(());
            }
        }
    }
    if root_resident(storage, key, history)
        || batch
            .owners
            .get_or_insert_with(|| OwnerManifests::read(storage))
            .protect(storage, key, true)?
        || protected(&cache)?
    {
        report.skipped_protected += 1;
        return Ok(());
    }
    if crate::root_cache::live_scope_keys_for_storage(storage).contains(key) {
        report.skipped_live += 1;
        return Ok(());
    }
    let phase = Instant::now();
    validate_tree(&cache, deadline, false)?;
    *report
        .timings_ms
        .entry("due_validation".into())
        .or_default() += phase.elapsed().as_millis() as u64;
    batch.items.push(PreparedDue {
        directory: directory.to_path_buf(),
        key: key.to_owned(),
        leases,
        due_lane: batch.due_lane,
        deadline,
    });
    Ok(())
}

pub(crate) fn remove_directory_if_closed(cache: &Path) -> io::Result<()> {
    tree_files(cache, Instant::now() + PASS_BUDGET)?;
    let _files = crate::db::file_identity::filesystem_guard();
    if crate::db::file_identity::has_open_connections_under(cache) {
        return Err(io::Error::other("open SQLite cache retained"));
    }
    fs::remove_dir_all(cache)
}

pub(crate) fn retention_tree_bytes(directory: &Path, deadline: Instant) -> io::Result<u64> {
    tree_files(directory, deadline).map(|files| files.iter().map(|(_, bytes)| bytes).sum())
}

fn eligible(
    storage: &Path,
    key: &str,
    history: &BTreeMap<String, Vec<Binding>>,
    now: u64,
    unknown_grace_ms: u64,
    report: &mut SweepReport,
) -> io::Result<bool> {
    if let Some(records) = history.get(key) {
        if records
            .iter()
            .any(|binding| !matches!(binding.root.try_exists(), Ok(false)))
        {
            report.skipped_live += 1;
            return Ok(false);
        }
        if records.iter().all(|binding| missing_and_old(binding, now)) {
            return Ok(true);
        }
        if records.iter().any(|binding| !mount_matches(binding)) {
            report.skipped_mount_mismatch += 1;
        } else {
            report.skipped_recent += 1;
        }
        return Ok(false);
    }
    let path = storage
        .join("retention/unknown")
        .join(format!("{key}.json"));
    // The first observation is an on-disk clock, not scheduling state. Reload
    // it on every pass so daemon replacement cannot restart the root grace.
    let first_seen = match read_json::<u64>(&path) {
        Ok(first_seen) => first_seen,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            atomic_json(&path, &now)?;
            now
        }
        Err(error) => return Err(error),
    };
    if now.saturating_sub(first_seen) >= unknown_grace_ms {
        return Ok(true);
    }
    report.skipped_recent += 1;
    Ok(false)
}

/// The view scopes every family registry lists, read at most once per pass
/// and only when a pass meets a view folder with no binding history.
#[derive(Default)]
struct ViewMembers(Option<Result<HashSet<String>, String>>);

impl ViewMembers {
    /// `Err` when any family registry could not be read completely: a scope
    /// that registry might list is then unknown, never absent.
    fn contains(&mut self, storage: &Path, scope: &str) -> Result<bool, String> {
        self.0
            .get_or_insert_with(|| view_member_scopes(storage).map_err(|error| error.to_string()))
            .as_ref()
            .map(|scopes| scopes.contains(scope))
            .map_err(Clone::clone)
    }
}

fn view_member_scopes(storage: &Path) -> io::Result<HashSet<String>> {
    let mut scopes = HashSet::new();
    for entry in bounded_dirs(&storage.join("blobs/v2"), TREE_ENTRIES)? {
        if !entry
            .path()
            .join(crate::views::registry::REGISTRY_FILE)
            .exists()
        {
            continue;
        }
        let family = entry.file_name().to_string_lossy().into_owned();
        let registry = crate::views::registry::FamilyRegistry::open_existing(storage, &family)
            .map_err(|error| io::Error::other(format!("family registry {family}: {error}")))?
            .ok_or_else(|| io::Error::other(format!("family registry {family} vanished")))?;
        let members = registry
            .members_bounded(HISTORY_ENTRIES + 1)
            .map_err(|error| io::Error::other(format!("family registry {family}: {error}")))?;
        if members.len() > HISTORY_ENTRIES {
            return Err(io::Error::other(format!(
                "family registry {family} lists more than {HISTORY_ENTRIES} views"
            )));
        }
        scopes.extend(members.into_iter().map(|member| member.scope));
    }
    Ok(scopes)
}

fn candidate(
    storage: &Path,
    directory: &Path,
    key: &str,
    flat: bool,
    history: &BTreeMap<String, Vec<Binding>>,
    now: u64,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    batch: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<()> {
    let before = DomainReport::from_report(report);
    let mut orphan = false;
    let result = candidate_inner(
        storage,
        directory,
        key,
        flat,
        history,
        now,
        deadline,
        cancelled,
        members,
        batch,
        &mut orphan,
        report,
    );
    if orphan {
        let mut delta = DomainReport::from_report(report).difference(&before);
        delta.examined += 1;
        if result.is_err() {
            // The caller adds one to skipped_protected for a candidate that
            // returned an error, after this wrapper has run. Add the same
            // count here so the orphan_views figures stay a subset of the
            // pass totals rather than disagreeing with them.
            delta.skipped_protected += 1;
        }
        report.orphan_views.add(&delta);
    }
    result
}

fn candidate_inner(
    storage: &Path,
    directory: &Path,
    key: &str,
    flat: bool,
    history: &BTreeMap<String, Vec<Binding>>,
    now: u64,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    batch: &mut DueBatch,
    orphan: &mut bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    let cache = if flat {
        directory.to_path_buf()
    } else {
        directory.join(key)
    };
    if !flat
        && history
            .get(key)
            .is_some_and(|records| records.iter().all(|binding| missing_and_old(binding, now)))
    {
        return prepare_due_cache(storage, directory, key, history, deadline, batch, report);
    }
    let before = tree_files(&cache, deadline)?;
    if directory.file_name().is_some_and(|name| name == "views") {
        if let Some(store) = crate::views::ViewStore::existing_dir(cache.clone()) {
            report.removed_generations += store
                .sweep_generations()
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
    }
    let after_views = tree_files(&cache, deadline)?;
    report.removed_bytes += before
        .iter()
        .map(|(_, bytes)| bytes)
        .sum::<u64>()
        .saturating_sub(after_views.iter().map(|(_, bytes)| bytes).sum());
    let mut leases = Vec::new();
    let local_writer = crate::root_cache::local_callgraph_writer(&cache);
    for name in [
        "writer.lease",
        "cache.lock",
        "symbols.lock",
        "semantic.lock",
    ] {
        if name == "writer.lease"
            && directory
                .file_name()
                .is_some_and(|domain| domain == "callgraph")
            && local_writer.is_some()
        {
            continue;
        }
        match crate::fs_lock::try_acquire(&cache.join(name), Duration::ZERO) {
            Ok(lease) => leases.push(lease),
            Err(_) => {
                report.skipped_protected += 1;
                return Ok(());
            }
        }
    }
    let before = tree_files(&cache, deadline)?;
    if directory
        .file_name()
        .is_some_and(|name| name == "callgraph")
    {
        let pointer = cache.join(format!("{key}.current"));
        if let Ok(current) = fs::read_to_string(pointer) {
            report.removed_generations +=
                crate::callgraph_store::gc_old_generations(&cache, key, current.trim());
        }
    }
    let after = tree_files(&cache, deadline)?;
    let old_bytes: u64 = before.iter().map(|(_, bytes)| bytes).sum();
    let new_bytes: u64 = after.iter().map(|(_, bytes)| bytes).sum();
    report.removed_bytes += old_bytes.saturating_sub(new_bytes);
    // Residency protects the root, not unheld obsolete generations. An owner
    // can be alive while using a different plane in the same repository family.
    if crate::root_cache::protected_read_marker_exists(&storage.join("retention"), key)
        || history.get(key).is_some_and(|records| {
            records.iter().any(|binding| {
                crate::root_cache::protected_read_marker_exists(
                    &storage.join("retention"),
                    &crate::path_identity::project_scope_key(&binding.root),
                )
            })
        })
    {
        report.skipped_protected += 1;
        return Ok(());
    }
    if crate::root_cache::live_scope_keys_for_storage(storage).contains(key) {
        report.skipped_live += 1;
        return Ok(());
    }
    for owner in bounded_dirs(&storage.join("artifact-owners"), HISTORY_ENTRIES)? {
        let path = owner.path().join("owner.json");
        if !path.exists() {
            continue;
        }
        let metadata: crate::artifact_owner::ArtifactOwnerManifest = read_json(&path)?;
        if ((!history.contains_key(key) && !owner_has_residency_marker(storage, &metadata))
            || owner.file_name() == key
            || metadata.project_scope_key == key)
            && crate::artifact_owner::protected_for_retention(&metadata)
        {
            report.skipped_protected += 1;
            return Ok(());
        }
    }
    // A checkout view folder with neither binding history nor a registry
    // member is an orphan: it gets a short observation grace instead of the
    // week reserved for keys whose checkout might still come back. A folder a
    // registry lists keeps the week; an unreadable registry keeps everything.
    let mut unknown_grace_ms = ROOT_GRACE_MS;
    if !flat
        && directory.file_name().is_some_and(|name| name == "views")
        && !history.contains_key(key)
    {
        match members.contains(storage, key) {
            Ok(true) => {}
            Ok(false) => {
                *orphan = true;
                unknown_grace_ms = ORPHAN_VIEW_GRACE_MS;
            }
            Err(error) => {
                *orphan = true;
                report.skipped_unknown += 1;
                if report.errors.len() < 16 {
                    report
                        .errors
                        .push(format!("{}: orphan check: {error}", cache.display()));
                }
                return Ok(());
            }
        }
    }
    if !eligible(storage, key, history, now, unknown_grace_ms, report)? {
        return Ok(());
    }
    if !history.contains_key(key)
        && after_views.iter().any(|(path, _)| {
            path.file_name().is_none_or(|name| {
                !matches!(
                    name.to_str(),
                    Some("writer.lease" | "cache.lock" | "semantic.lock" | "symbols.lock")
                )
            }) && fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                // A file whose modification time cannot be read counts as
                // recent for an orphan, so the orphan is kept: the file may be
                // changing right now. For any other key this check runs only
                // after the week-long observation grace has passed, and an
                // undatable file (usually one removed since the walk) is
                // treated as old, as it was before the orphan rule existed.
                .map_or(*orphan, |mtime| {
                    std::time::SystemTime::now()
                        .duration_since(mtime)
                        .unwrap_or_default()
                        < Duration::from_millis(unknown_grace_ms)
                })
        })
    {
        report.skipped_recent += 1;
        return Ok(());
    }
    // Walks run without either admission or SQLite's process-wide open gate.
    // Recheck protection and binding history under admission immediately before
    // deletion, since a configure or query may have arrived during the walks.
    // A registration binds its root under this barrier before it creates a
    // view folder, so an orphan that gained history here is no longer one.
    let _barrier = barrier(storage)?;
    let current_history = bindings(storage)?;
    if !eligible(
        storage,
        key,
        &current_history,
        now,
        unknown_grace_ms,
        report,
    )? || crate::root_cache::live_scope_keys_for_storage(storage).contains(key)
        || protected(&cache)?
    {
        report.skipped_protected += 1;
        return Ok(());
    }
    let files_guard = crate::db::file_identity::filesystem_guard();
    if crate::db::file_identity::has_open_connections_under(&cache) {
        report.skipped_protected += 1;
        return Ok(());
    }
    if cancelled() {
        report.cancelled = true;
        return Ok(());
    }
    if flat {
        // Legacy domains contain several roots. Delete only the key's file set,
        // never the directory or another key's files/coordination records.
        for (path, bytes) in after {
            if path.parent() == Some(cache.as_path())
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&format!("{key}.")))
            {
                fs::remove_file(path)?;
                report.removed_bytes += bytes;
            }
        }
    } else {
        retire_directory(&cache)?;
        report.removed_bytes += after_views
            .iter()
            .map(|(_, bytes)| bytes)
            .sum::<u64>()
            .min(new_bytes);
    }
    report.removed_roots += 1;
    // The observation is shared by every plane for this key. Keep its original
    // grace until the last cache is gone, rather than restarting it per plane.
    if !cache_key_present(storage, key, deadline)? {
        clear_unknown_observation(storage, key)?;
    }
    drop(files_guard);
    drop(leases);
    Ok(())
}

pub(crate) fn run_pass(storage: &Path, cancelled: &dyn Fn() -> bool) -> SweepReport {
    run_pass_with_clock(storage, cancelled, &Instant::now)
}

/// Keys whose every binding is missing and past its grace, in key order.
fn due_keys(history: &BTreeMap<String, Vec<Binding>>, now: u64) -> Vec<String> {
    history
        .iter()
        .filter(|(key, records)| {
            key_valid(key)
                // Checking that every root is gone takes one stat per record,
                // so it runs first. missing_and_old may also probe the volume
                // the root was on, and runs only for keys that pass it.
                && records
                    .iter()
                    .all(|binding| matches!(binding.root.try_exists(), Ok(false)))
                && records.iter().all(|binding| missing_and_old(binding, now))
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Runs the full candidate checks on the checkout caches of keys that are
/// already due, starting after the key the previous pass ended on.
fn due_first(
    storage: &Path,
    domains: &[(PathBuf, bool)],
    history: &BTreeMap<String, Vec<Binding>>,
    deadline: Instant,
    clock: &dyn Fn() -> Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    batch: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<()> {
    let mut caches = Vec::new();
    let phase = Instant::now();
    let keys = due_keys(history, report.at_ms);
    report
        .timings_ms
        .insert("due_keys".into(), phase.elapsed().as_millis() as u64);
    for key in keys {
        let mut present = false;
        for (directory, flat) in domains {
            // A flat domain keeps the files of many keys side by side in one
            // directory, with no folder per key, so this per-key folder check
            // does not apply. The lexical walk handles those file sets.
            if *flat {
                continue;
            }
            if fs::symlink_metadata(directory.join(&key)).is_ok_and(|metadata| metadata.is_dir()) {
                present = true;
                if directory == &storage.join("views") {
                    report.due_views_remaining += 1;
                }
                caches.push((key.clone(), directory.clone()));
            }
        }
        report.due_keys += usize::from(present);
    }
    let cursor = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(storage)
        .and_then(|state| state.due_cursor.clone());
    // Large per-checkout views dominate disk usage. Process them before other
    // planes, retaining a rotating key order within each priority class.
    caches.sort_by_key(|(key, directory)| {
        (
            directory != &storage.join("views"),
            cursor.as_ref().is_some_and(|last| key <= last),
            key.clone(),
        )
    });
    let mut last = None;
    batch.due_lane = true;
    for (key, directory) in caches.into_iter().take(DUE_FIRST_ENTRIES) {
        if cancelled() {
            report.cancelled = true;
            break;
        }
        if clock() >= deadline {
            report.stopped_early = true;
            break;
        }
        let before = DomainReport::from_report(report);
        report.examined += 1;
        if let Err(error) = candidate(
            storage,
            &directory,
            &key,
            false,
            history,
            report.at_ms,
            deadline,
            cancelled,
            members,
            batch,
            report,
        ) {
            report.skipped_protected += 1;
            if report.errors.len() < 16 {
                report
                    .errors
                    .push(format!("{}: {error}", directory.join(&key).display()));
            }
        }
        let delta = DomainReport::from_report(report).difference(&before);
        report.due_first.add(&delta);
        report
            .domains
            .entry(
                directory
                    .strip_prefix(storage)
                    .map_err(io::Error::other)?
                    .to_string_lossy()
                    .into_owned(),
            )
            .or_default()
            .add(&delta);
        last = Some(key);
        if batch.full() {
            batch.flush(storage, report.at_ms, cancelled, report)?;
        }
    }
    let flushed = batch.flush(storage, report.at_ms, cancelled, report);
    batch.due_lane = false;
    flushed?;
    if let Some(last) = last {
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(storage.to_path_buf())
            .or_default()
            .due_cursor = Some(last);
    }
    Ok(())
}

fn run_pass_with_clock(
    storage: &Path,
    cancelled: &dyn Fn() -> bool,
    clock: &dyn Fn() -> Instant,
) -> SweepReport {
    run_pass_with_budget(storage, cancelled, clock, PASS_BUDGET)
}

fn persist_cursor(storage: &Path, path: &Path, cursor: &TraversalCursor) -> io::Result<()> {
    #[cfg(test)]
    test_hook(storage, "cursor-commit");
    #[cfg(not(test))]
    let _ = storage;
    atomic_json(path, cursor)
}

/// Unbound views have their own cursor and reserved budget. A long due queue
/// or a lexical batch full of bound views must not postpone orphan observation.
fn orphan_first(
    storage: &Path,
    history: &BTreeMap<String, Vec<Binding>>,
    deadline: Instant,
    clock: &dyn Fn() -> Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    due: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<HashSet<PathBuf>> {
    let batch = traversal_batch_filtered(
        storage,
        &[(storage.join("views"), false)],
        "orphans",
        PASS_ENTRIES,
        &|entry| {
            let key = entry.file_name().to_string_lossy().into_owned();
            key_valid(&key) && !history.contains_key(&key)
        },
    )?;
    let mut visited = HashSet::new();
    let mut last = None;
    let mut completed = true;
    for item in batch.entries {
        if cancelled() {
            report.cancelled = true;
            completed = false;
            break;
        }
        if clock() >= deadline {
            report.stopped_early = true;
            completed = false;
            break;
        }
        let before = DomainReport::from_report(report);
        report.examined += 1;
        let key = item.entry.file_name().to_string_lossy().into_owned();
        if item.entry.file_type()?.is_dir() {
            if let Err(error) = candidate(
                storage,
                &item.directory,
                &key,
                false,
                history,
                report.at_ms,
                deadline,
                cancelled,
                members,
                due,
                report,
            ) {
                report.skipped_protected += 1;
                if report.errors.len() < 16 {
                    report
                        .errors
                        .push(format!("{}: {error}", item.entry.path().display()));
                }
            }
        }
        let delta = DomainReport::from_report(report).difference(&before);
        report
            .domains
            .entry("views".into())
            .or_default()
            .add(&delta);
        visited.insert(item.entry.path());
        last = Some(item.after);
    }
    // Views this lane visits have no binding history, so none is due; flush
    // anyway so nothing prepared here can outlive the lane.
    due.flush(storage, report.at_ms, cancelled, report)?;
    if completed {
        last = Some(batch.end);
    }
    if let Some(cursor) = last {
        persist_cursor(storage, &batch.cursor_path, &cursor)?;
    }
    Ok(visited)
}

/// One pass with an explicit time budget. Production always passes
/// PASS_BUDGET; tests that check the entry bound pass a budget no runner can
/// exhaust, so a slow machine cannot stop the pass by wall clock first.
fn run_pass_with_budget(
    storage: &Path,
    cancelled: &dyn Fn() -> bool,
    clock: &dyn Fn() -> Instant,
    budget: Duration,
) -> SweepReport {
    let started = Instant::now();
    let mut report = SweepReport {
        at_ms: crate::pins::now_ms(),
        ..Default::default()
    };
    let result = (|| -> io::Result<()> {
        if !crate::root_cache::storage_allows_root_keyed(storage)? {
            return Err(io::Error::other("network storage retained"));
        }
        prune_bindings(storage, cancelled, &mut report)?;
        let history = bindings(storage)?;
        let domains = domain_paths(storage)?;
        for (directory, _) in &domains {
            report
                .domains
                .entry(
                    directory
                        .strip_prefix(storage)
                        .map_err(io::Error::other)?
                        .to_string_lossy()
                        .into_owned(),
                )
                .or_default();
        }
        let deadline = Instant::now() + budget;
        report
            .timings_ms
            .insert("setup".into(), started.elapsed().as_millis() as u64);
        let phase = Instant::now();
        let mut members = ViewMembers::default();
        let mut due = DueBatch::default();
        // Keys already past their grace go first, with at most half the
        // budget, so their reclaim does not wait for the lexical walk to come
        // around. That walk visits every cache in every domain and takes days
        // at the rate new checkouts arrive.
        due_first(
            storage,
            &domains,
            &history,
            deadline - budget / 2,
            clock,
            cancelled,
            &mut members,
            &mut due,
            &mut report,
        )?;
        report
            .timings_ms
            .insert("due_first".into(), phase.elapsed().as_millis() as u64);
        if report.cancelled {
            return Ok(());
        }
        let phase = Instant::now();
        let orphan_visited = orphan_first(
            storage,
            &history,
            deadline - budget / 4,
            clock,
            cancelled,
            &mut members,
            &mut due,
            &mut report,
        )?;
        report
            .timings_ms
            .insert("orphan_views".into(), phase.elapsed().as_millis() as u64);
        let phase = Instant::now();
        let batch = next_batch(storage, &domains)?;
        report.stopped_early |= batch.entries.len() == PASS_ENTRIES;
        let mut completed = true;
        let mut last_processed = None;
        for item in batch.entries {
            let TraversalEntry {
                directory,
                flat,
                entry,
                after,
            } = item;
            if cancelled() {
                report.cancelled = true;
                completed = false;
                break;
            }
            if clock() >= deadline {
                report.stopped_early = true;
                completed = false;
                break;
            }
            if orphan_visited.contains(&entry.path()) {
                last_processed = Some(after);
                continue;
            }
            let before = DomainReport::from_report(&report);
            report.examined += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let key = if flat {
                name.split('.').next().unwrap_or_default()
            } else {
                &name
            };
            if key_valid(key)
                && (flat || entry.file_type()?.is_dir())
                && (!flat || name.ends_with(".current") || name == format!("{key}.sqlite"))
            {
                if let Err(error) = candidate(
                    storage,
                    &directory,
                    key,
                    flat,
                    &history,
                    report.at_ms,
                    deadline,
                    cancelled,
                    &mut members,
                    &mut due,
                    &mut report,
                ) {
                    report.skipped_protected += 1;
                    if report.errors.len() < 16 {
                        report
                            .errors
                            .push(format!("{}: {error}", entry.path().display()));
                    }
                }
            }
            let delta = DomainReport::from_report(&report).difference(&before);
            report
                .domains
                .entry(after.domain.to_string_lossy().into_owned())
                .or_default()
                .add(&delta);
            last_processed = Some(after);
            if due.full() {
                due.flush(storage, report.at_ms, cancelled, &mut report)?;
            }
        }
        due.flush(storage, report.at_ms, cancelled, &mut report)?;
        if completed {
            last_processed = Some(batch.end);
        }
        // One durable commit per batch is enough: a crash can repeat checked
        // entries, but cannot skip the unexamined tail of a prefetched batch.
        if let Some(cursor) = last_processed {
            persist_cursor(storage, &batch.cursor_path, &cursor)?;
        }
        report
            .timings_ms
            .insert("roots".into(), phase.elapsed().as_millis() as u64);
        schedule_reaper(storage);
        Ok(())
    })();
    if let Err(error) = result {
        report.errors.push(error.to_string());
    }
    // Blob stores have their own mark-before-sweep barrier. Release the root
    // barrier before entering it; otherwise pin admission would deadlock.
    if !report.cancelled && !cancelled() {
        let phase = Instant::now();
        if let Err(error) = collect_one_family(storage, cancelled, &mut report) {
            report.errors.push(error.to_string());
        }
        report
            .timings_ms
            .insert("blobs_v1".into(), phase.elapsed().as_millis() as u64);
        let phase = Instant::now();
        if let Err(error) = collect_one_v2_family(storage, cancelled, &mut report) {
            report.errors.push(error.to_string());
        }
        report
            .timings_ms
            .insert("blobs_v2".into(), phase.elapsed().as_millis() as u64);
    }
    report
        .timings_ms
        .insert("total".into(), started.elapsed().as_millis() as u64);
    report.cancelled |= cancelled();
    let mut states = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states.entry(storage.to_path_buf()).or_default();
    if report.cancelled {
        state.last_run = None;
    }
    state.report = Some(report.clone());
    report
}

fn collect_one_v2_family(
    storage: &Path,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    visit_one_family(storage, "blobs/v2", "families-v2", cancelled, |entry| {
        let family = entry.file_name().to_string_lossy().into_owned();
        if !key_valid(&family) || !entry.file_type()?.is_dir() || cancelled() {
            return Ok(());
        }
        let registry = crate::views::registry::FamilyRegistry::open_existing(storage, &family)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let Some(registry) = registry else {
            return Ok(());
        };
        let deadline = Instant::now() + PASS_BUDGET;
        for member in registry
            .members_bounded(HISTORY_ENTRIES)
            .map_err(|error| io::Error::other(error.to_string()))?
        {
            if cancelled() || Instant::now() >= deadline {
                report.stopped_early = true;
                return Ok(());
            }
            // V2 pin admission uses this same registry barrier. It cannot race the
            // generation protection snapshot and removal under the pointer lock.
            if let Some(store) = crate::views::ViewStore::existing_dir(member.view_dir) {
                let removed = registry
                    .with_barrier(|_| {
                        store
                            .sweep_generations_locked()
                            .map_err(crate::views::registry::RegistryError::View)
                    })
                    .map_err(|error| io::Error::other(error.to_string()))?;
                report.removed_generations += removed;
            }
        }
        if cancelled() {
            return Ok(());
        }
        let swept = crate::gc::family::sweep_family_bounded(
            &registry,
            None,
            crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
            crate::gc::family::SweepBounds {
                max_rows_per_store: 4096,
                max_entries_per_dir: 4096,
                deadline: Some(deadline),
            },
            None,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        report.removed_roots += swept.deregistered.len();
        report.examined += swept.examined_roots;
        report.skipped_live += swept.skipped_live;
        report.skipped_recent += swept.skipped_recent;
        report.skipped_mount_mismatch += swept.skipped_mount_mismatch;
        report.skipped_protected += swept.skipped_protected;
        report.removed_bytes += swept.removed_root_bytes;
        report.unmeasured_removed_roots += swept.unmeasured_removed_roots;
        report
            .domains
            .entry("views/v2".to_owned())
            .or_default()
            .add(&DomainReport {
                examined: swept.examined_roots,
                skipped_live: swept.skipped_live,
                skipped_recent: swept.skipped_recent,
                skipped_mount_mismatch: swept.skipped_mount_mismatch,
                skipped_protected: swept.skipped_protected,
                skipped_unknown: 0,
                removed_roots: swept.deregistered.len(),
                removed_bytes: swept.removed_root_bytes,
                unmeasured_removed_roots: swept.unmeasured_removed_roots,
            });
        report
            .domains
            .entry("blobs/v2".to_owned())
            .or_default()
            .add(&DomainReport {
                examined: swept.examined_blobs,
                removed_bytes: swept.deleted_bytes,
                ..Default::default()
            });
        report.removed_blobs += swept.deleted_blobs;
        report.examined_blobs += swept.examined_blobs;
        report.removed_blob_payload_bytes += swept.deleted_bytes;
        report.blob_reclaim_deferred |= swept.reclaim_deferred;
        report.stopped_early |= swept.stopped_early;
        Ok(())
    })
}

#[cfg(test)]
mod storage_retention_tests {
    use super::*;

    #[test]
    fn retention_observation_clears_only_after_last_cache_plane() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        for domain in ["symbols", "semantic"] {
            let path = cache(temp.path(), domain, key);
            filetime::set_file_mtime(
                path.join("payload.bin"),
                filetime::FileTime::from_unix_time(0, 0),
            )
            .unwrap();
        }
        atomic_json(
            &temp.path().join(format!("retention/unknown/{key}.json")),
            &0_u64,
        )
        .unwrap();
        assert_eq!(
            run_pass_entry_bound(temp.path()).removed_roots,
            2,
            "removing one plane reset the shared grace for the other plane"
        );
        assert!(!temp
            .path()
            .join(format!("retention/unknown/{key}.json"))
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn retention_private_permissions_survive_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD_PATH: &str = "AFT_RETENTION_PRIVATE_UMASK_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            unsafe {
                libc::umask(0o777);
            }
            atomic_json(&path, &serde_json::json!({"bound": true})).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                read_json::<serde_json::Value>(&path).unwrap()["bound"],
                true
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private.json");
        let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "storage_retention::storage_retention_tests::retention_private_permissions_survive_restrictive_umask"]).env(CHILD_PATH, &path).output().unwrap();
        assert!(
            output.status.success(),
            "restrictive-umask writer failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn retention_worker_panic_releases_running_state() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().to_path_buf();
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                storage.clone(),
                State {
                    running: true,
                    last_run: Some(Instant::now()),
                    ..State::default()
                },
            );
        assert!(
            std::panic::catch_unwind(|| scheduled_run(&storage, || panic!(
                "injected retention panic"
            )))
            .is_err()
        );
        let states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!states[&storage].running, "panic stranded the running flag");
        assert!(
            states[&storage].last_run.is_none(),
            "failed pass must permit rescheduling"
        );
    }

    #[test]
    fn retention_record_is_private_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binding.json");
        crate::durability::take();
        atomic_json(&path, &serde_json::json!({"bound": true})).unwrap();
        let events = crate::durability::take();
        assert!(events
            .iter()
            .any(|(kind, _)| *kind == crate::durability::EventKind::FileSync));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                events.last(),
                Some(&(
                    crate::durability::EventKind::DirectorySync,
                    dir.path().to_path_buf()
                ))
            );
        }
    }

    /// A budget no test runner exhausts. Tests that count entries per pass
    /// check the entry bound, not the runner's speed; the time bound has its
    /// own tests with an injected clock or a zero budget.
    const NO_TIME_BOUND: Duration = Duration::from_secs(24 * 60 * 60);

    fn run_pass_entry_bound(storage: &Path) -> SweepReport {
        run_pass_with_budget(storage, &|| false, &Instant::now, NO_TIME_BOUND)
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        record_bind(temp.path(), &root, &scope).unwrap();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.last_bound_ms = 0;
        atomic_json(&record, &binding).unwrap();
        (temp, root, scope)
    }

    fn cache(storage: &Path, domain: &str, key: &str) -> PathBuf {
        let path = storage.join(domain).join(key);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("payload.bin"), b"rebuildable").unwrap();
        path
    }

    struct TestVolumeUuidGuard(PathBuf);

    impl Drop for TestVolumeUuidGuard {
        fn drop(&mut self) {
            test_volume_uuids()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.0);
        }
    }

    // Give the surviving parent a UUID even on runners without filesystem UUID
    // discovery. Disconnection fixtures must invalidate this identity as well
    // as their device number, or they still describe the mounted parent volume.
    fn uuid_backed_binding(storage: &Path, scope: &str) -> TestVolumeUuidGuard {
        let record = storage.join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        let parent = binding.root.parent().unwrap().to_path_buf();
        assert!(parent.is_dir());
        let uuid = format!("fixture-volume-{scope}");
        test_volume_uuids()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.clone(), uuid.clone());
        let guard = TestVolumeUuidGuard(parent);
        binding.stable_volume = Some(VolumeIdentity {
            uuid: Some(uuid),
            boot_id: boot_id(),
        });
        atomic_json(&record, &binding).unwrap();
        assert!(
            mount_matches(&binding),
            "fixture parent must expose its recorded UUID"
        );
        guard
    }

    fn disconnect_uuid_backed_binding(storage: &Path, scope: &str, anchor: &Path) -> Binding {
        let record = storage.join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.volume = Some((anchor.to_path_buf(), 42));
        // The removed volume's UUID is absent at both the unreachable anchor
        // and the surviving parent, which belongs to the fixture's mounted volume.
        binding.stable_volume.as_mut().unwrap().uuid = Some(format!("disconnected-volume-{scope}"));
        atomic_json(&record, &binding).unwrap();
        binding
    }

    #[test]
    fn traversal_resumes_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..130 {
            cache(temp.path(), "inspect", &format!("{i:016x}"));
        }
        let first = run_pass_entry_bound(temp.path());
        assert_eq!(first.examined, PASS_ENTRIES, "{first:?}");
        states().lock().unwrap().remove(temp.path());
        let second = run_pass_entry_bound(temp.path());
        assert_eq!(second.examined, PASS_ENTRIES, "{second:?}");
        assert_eq!(
            fs::read_dir(temp.path().join("retention/unknown"))
                .unwrap()
                .count(),
            128
        );
    }

    #[test]
    fn traversal_preserves_unprocessed_batch_tail() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..10 {
            cache(temp.path(), "inspect", &format!("{i:016x}"));
        }
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let now = Instant::now();
        let first = run_pass_with_clock(temp.path(), &|| false, &|| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                now
            } else {
                now + PASS_BUDGET * 2
            }
        });
        assert_eq!(first.examined, 1, "{first:?}");
        let second = run_pass_entry_bound(temp.path());
        assert_eq!(second.examined, 9, "{second:?}");
        let cursor: TraversalCursor =
            read_json(&temp.path().join("retention/cursors/roots.json")).unwrap();
        assert!(
            cursor.last_processed.is_none(),
            "completed lap did not reset"
        );
    }

    #[test]
    fn family_traversal_resumes_after_restart_for_both_layouts() {
        let temp = tempfile::tempdir().unwrap();
        for (domain, kind) in [("blobs", "families-v1"), ("blobs/v2", "families-v2")] {
            for i in 0..3 {
                fs::create_dir_all(temp.path().join(domain).join(format!("{i:016x}"))).unwrap();
            }
            let mut visited = Vec::new();
            for _ in 0..2 {
                visit_one_family(temp.path(), domain, kind, &|| false, |entry| {
                    visited.push(entry.file_name());
                    Ok(())
                })
                .unwrap();
                states().lock().unwrap().remove(temp.path());
            }
            assert_eq!(
                visited,
                ["0000000000000000", "0000000000000001"].map(std::ffi::OsString::from)
            );
        }
    }

    #[test]
    fn stable_volume_change_with_parent_continuity_is_eligible() {
        let (temp, root, scope) = fixture();
        let mut binding: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        binding.volume = Some((temp.path().join("old-anchor"), 10));
        binding.stable_volume = Some(VolumeIdentity {
            uuid: Some("original-volume".to_owned()),
            boot_id: Some("old-boot".to_owned()),
        });
        fs::remove_dir(&root).unwrap();
        let mounted = mount_matches_with(
            &binding,
            &|path| {
                Some(
                    if Some(path) == root.parent() {
                        "original-volume"
                    } else {
                        "replacement-volume"
                    }
                    .to_owned(),
                )
            },
            &|_| Some(11),
            Some("new-boot"),
            false,
        );
        assert_eq!(
            binding_eligibility_with_mount(&binding, ROOT_GRACE_MS, mounted),
            RootEligibility::Due
        );
    }

    #[test]
    fn stable_volume_replacement_is_blocked_even_if_device_is_reused() {
        let (temp, root, scope) = fixture();
        let mut binding: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        binding.volume = Some((temp.path().to_path_buf(), 10));
        binding.stable_volume = Some(VolumeIdentity {
            uuid: Some("original-volume".to_owned()),
            boot_id: Some("old-boot".to_owned()),
        });
        fs::remove_dir(&root).unwrap();
        let mounted = mount_matches_with(
            &binding,
            &|_| Some("replacement-volume".to_owned()),
            &|_| Some(10),
            Some("new-boot"),
            false,
        );
        assert_eq!(
            binding_eligibility_with_mount(&binding, ROOT_GRACE_MS, mounted),
            RootEligibility::MountMismatch
        );
    }

    #[test]
    fn device_fallback_requires_same_boot_and_device() {
        let (temp, _root, scope) = fixture();
        let mut binding: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        binding.volume = Some((temp.path().to_path_buf(), 10));
        binding.stable_volume = Some(VolumeIdentity {
            uuid: None,
            boot_id: Some("same-boot".to_owned()),
        });
        assert!(mount_matches_with(
            &binding,
            &|_| None,
            &|_| Some(10),
            Some("same-boot"),
            false
        ));
        assert!(!mount_matches_with(
            &binding,
            &|_| None,
            &|_| Some(10),
            Some("new-boot"),
            false
        ));
        assert!(!mount_matches_with(
            &binding,
            &|_| None,
            &|_| Some(11),
            Some("same-boot"),
            false
        ));
    }

    #[test]
    fn sweep_reports_examined_and_skip_reasons_per_domain() {
        let temp = tempfile::tempdir().unwrap();
        for (i, live, mismatch) in [(0, false, false), (1, true, false), (2, false, true)] {
            let root = temp.path().join(format!("checkout-{i}"));
            fs::create_dir_all(&root).unwrap();
            let scope = crate::path_identity::project_scope_key(&root);
            record_bind(temp.path(), &root, &scope).unwrap();
            let record = temp.path().join(format!("retention/roots/{scope}.json"));
            let mut binding: Binding = read_json(&record).unwrap();
            binding.last_bound_ms = 0;
            if mismatch {
                binding.volume = Some((temp.path().join("Volumes/X"), u64::MAX));
                binding.stable_volume = None;
            }
            atomic_json(&record, &binding).unwrap();
            cache(temp.path(), "inspect", &scope);
            if !live {
                fs::remove_dir(root).unwrap();
            }
        }
        cache(temp.path(), "inspect", "0000000000000000");
        let report = run_pass_entry_bound(temp.path());
        let domain = &report.domains["inspect"];
        assert_eq!(
            (
                domain.examined,
                domain.skipped_live,
                domain.skipped_recent,
                domain.skipped_mount_mismatch,
                domain.removed_roots
            ),
            (4, 1, 1, 1, 1),
            "{report:?}"
        );
        assert_eq!(domain.removed_bytes, 0);
        assert_eq!(domain.unmeasured_removed_roots, 1);
        let log = serde_json::to_value(&report).unwrap();
        assert_eq!(log["domains"]["inspect"]["skipped_mount_mismatch"], 1);
    }

    #[test]
    fn unproven_mount_change_is_blocked_and_counted() {
        let (temp, root, scope) = fixture();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.volume = Some((temp.path().join("Volumes/X"), u64::MAX));
        binding.stable_volume = None;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        cache(temp.path(), "inspect", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert_eq!(report.domains["inspect"].skipped_mount_mismatch, 1);
        assert_eq!(report.skipped_recent, 0, "{report:?}");
        assert_eq!(report.removed_roots, 0);
        assert!(temp.path().join("inspect").join(scope).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_legacy_boot_device_change_is_eligible() {
        let (temp, root, scope) = fixture();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.volume = Some((PathBuf::from("/"), device(Path::new("/")).unwrap() + 1));
        binding.stable_volume = None;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        cache(temp.path(), "inspect", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(
            missing_root_due(temp.path(), &binding.root, 0),
            "{report:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_volume_identity_follows_data_volume_firmlinks() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = volume_anchor(temp.path()).unwrap().0;
        assert_eq!(anchor, Path::new("/System/Volumes/Data"));
        let uuid = volume_uuid(temp.path()).expect("APFS UUID unavailable");
        assert_eq!(volume_uuid(&anchor), Some(uuid.clone()));
        assert_eq!(volume_anchor(Path::new("/Users")).unwrap().0, anchor);
        assert_eq!(volume_uuid(Path::new("/Users")), Some(uuid));
        assert!(boot_id().is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_stable_uuid_survives_device_change() {
        let (temp, root, scope) = fixture();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        assert!(binding.stable_volume.as_ref().unwrap().uuid.is_some());
        binding.volume.as_mut().unwrap().1 += 1;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        cache(temp.path(), "inspect", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert_eq!(report.skipped_mount_mismatch, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unproven_external_device_change_is_blocked() {
        unproven_mount_change_is_blocked_and_counted();
    }

    #[test]
    fn missing_root_retention_finds_the_identity_recorded_while_present() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(&root).unwrap();
        let missing = crate::root_cache::canonical_root(&root);
        assert_eq!(missing_root_scope_key(&missing), scope);
        assert!(
            missing_root_due(temp.path(), &root, 0),
            "old missing root lost its durable binding: root={root:?} missing={missing:?} scope={scope}"
        );
    }

    #[test]
    fn startup_grace_does_not_admit_retention_or_blob_sweeps() {
        let temp = tempfile::tempdir().unwrap();
        let now = Instant::now();
        assert!(!startup_sweeps_ready_at(temp.path(), now));
        assert!(!startup_sweeps_ready_at(
            temp.path(),
            now + STARTUP_GRACE - Duration::from_nanos(1)
        ));
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(temp.path().to_path_buf()),
                ..Default::default()
            },
        );
        schedule(
            temp.path().to_path_buf(),
            ctx.subc_lifecycle_admission(),
            ctx.configure_generation_flag(),
            ctx.configure_generation(),
        );
        // Scheduling records admission synchronously, before spawning a worker.
        // Counting that record checks the actual scheduling lane without racing
        // the background worker or sleeping through the startup grace.
        let admitted = states()
            .lock()
            .unwrap()
            .get(temp.path())
            .unwrap()
            .last_run
            .iter()
            .count();
        assert_eq!(admitted, 0, "retention started during startup warm-up");
        assert!(startup_sweeps_ready_at(temp.path(), now + STARTUP_GRACE));
    }

    // Gate the maintenance worker at a named phase, without making the request
    // thread depend on scheduler speed or an artificial sleep.
    fn paused_pass(
        storage: &Path,
        cache: &Path,
        phase: &'static str,
    ) -> (
        std::thread::JoinHandle<SweepReport>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let once = std::sync::atomic::AtomicBool::new(false);
        test_hooks().lock().unwrap().insert(
            cache.to_path_buf(),
            Arc::new(move |step| {
                if step == phase && !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let storage = storage.to_path_buf();
        let cache = cache.to_path_buf();
        let worker = std::thread::spawn(move || {
            let report = run_pass(&storage, &|| false);
            test_hooks().lock().unwrap().remove(&cache);
            report
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        (worker, release_tx)
    }

    #[test]
    fn retention_walk_allows_view_load_without_lock_waits() {
        let (temp, _root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        fs::write(view.derived_path("old").unwrap(), b"generation").unwrap();
        let (worker, release) = paused_pass(temp.path(), view.view_dir(), "walk");
        let waits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = waits.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let loader = std::thread::spawn(move || {
            let _observer = crate::fs_lock::observe_retry_sleeps_for_test(observed);
            let result = crate::pins::QueryPin::acquire(view.view_dir(), "old")
                .map_err(|error| error.to_string())
                .and_then(|pin| {
                    view.current_generation_read_only()
                        .map(|generation| (pin, generation))
                        .map_err(|error| error.to_string())
                });
            done_tx.send(result).unwrap();
        });
        let loaded = done_rx.recv_timeout(Duration::from_secs(6));
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        loader.join().unwrap();
        let (pin, current) = loaded
            .expect("view load blocked by retention walk")
            .unwrap();
        assert_eq!(waits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(current, None);
        assert_eq!(report.removed_generations, 0, "{report:?}");
        drop(pin);
    }

    #[test]
    fn retention_rechecks_a_generation_pinned_during_the_walk() {
        let (temp, _root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        let old = view.derived_path("old").unwrap();
        fs::write(&old, b"generation").unwrap();
        let (worker, release) = paused_pass(temp.path(), view.view_dir(), "walk");
        let pin = crate::pins::QueryPin::acquire(view.view_dir(), "old");
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        let pin = pin.unwrap();
        assert_eq!(report.removed_generations, 0, "{report:?}");
        assert!(old.is_file());
        drop(pin);
        assert_eq!(run_pass(temp.path(), &|| false).removed_generations, 1);
    }

    #[test]
    fn retention_generation_delete_excludes_pin_admission() {
        let (temp, _root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        let old = view.derived_path("old").unwrap();
        fs::write(&old, b"generation").unwrap();
        let (worker, release) = paused_pass(temp.path(), view.view_dir(), "generation-delete");
        // Pin admission acquires this exact lock. A nonwaiting attempt must be
        // refused while the deletion is paused after its protection recheck.
        let admission =
            crate::fs_lock::try_acquire(&temp.path().join("retention/sweep.lock"), Duration::ZERO);
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        assert!(matches!(
            admission,
            Err(crate::fs_lock::AcquireError::Timeout)
        ));
        assert_eq!(report.removed_generations, 1, "{report:?}");
        assert!(!old.exists());
        assert!(crate::pins::QueryPin::acquire(view.view_dir(), "next").is_ok());
    }

    #[test]
    fn storage_retention_collects_generation_from_a_bound_root() {
        let (temp, root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        let old = view.derived_path("old").unwrap();
        fs::write(&old, b"generation").unwrap();
        crate::root_cache::register_live_scope(temp.path(), &root);
        let report = run_pass(temp.path(), &|| false);
        crate::root_cache::unregister_live_scope(temp.path(), &root);
        assert_eq!(report.removed_generations, 1, "{report:?}");
        assert!(!old.exists());
        assert!(view.view_dir().exists());
    }

    #[test]
    fn storage_retention_reclaims_old_missing_roots_in_every_domain() {
        let (temp, root, scope) = fixture();
        let paths = DOMAINS
            .iter()
            .map(|domain| cache(temp.path(), domain, &scope))
            .collect::<Vec<_>>();
        fs::remove_dir(root).unwrap();
        let report = run_pass_entry_bound(temp.path());
        assert_eq!(report.removed_roots, 6, "{report:?}");
        assert_eq!(report.removed_bytes, 0);
        assert_eq!(report.unmeasured_removed_roots, 6);
        assert!(paths.iter().all(|path| !path.exists()));
        assert_eq!(snapshot(temp.path()).unwrap().removed_roots, 6);
    }

    #[test]
    fn storage_retention_keeps_a_live_root_even_with_old_binding() {
        let (temp, _root, scope) = fixture();
        let path = cache(temp.path(), "semantic", &scope);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_keeps_a_recently_bound_missing_root() {
        let (temp, root, scope) = fixture();
        record_bind(temp.path(), &root, &scope).unwrap();
        fs::remove_dir(root).unwrap();
        let path = cache(temp.path(), "symbols", &scope);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    fn deleted_checkout_fixture(
        linked: bool,
        minutes: u64,
        legacy: bool,
        history_copies: bool,
    ) -> (tempfile::TempDir, PathBuf, String, PathBuf) {
        let (temp, root, scope) = bound_checkout_fixture(linked, minutes, legacy, history_copies);
        let path = cache(temp.path(), "views", &scope);
        fs::remove_dir_all(&root).unwrap();
        (temp, root, scope, path)
    }

    /// A checkout last bound `minutes` ago.
    fn bound_checkout_fixture(
        linked: bool,
        minutes: u64,
        legacy: bool,
        history_copies: bool,
    ) -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            let mut command = std::process::Command::new("git");
            crate::test_env::apply_hermetic_git_env(command.current_dir(&main));
            let output = command.args(args).output().unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        git(&["init", "--quiet"]);
        git(&[
            "-c",
            "user.name=Retention Tests",
            "-c",
            "user.email=retention@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let root = if linked {
            let worktree = temp.path().join("linked-checkout");
            git(&[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worktree.to_str().unwrap(),
            ]);
            worktree
        } else {
            main
        };
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        record_bind(temp.path(), &root, &scope).unwrap();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: serde_json::Value = read_json(&record).unwrap();
        let last_bound = crate::pins::now_ms() - minutes * 60 * 1000;
        binding["last_bound_ms"] = last_bound.into();
        if legacy {
            binding.as_object_mut().unwrap().remove("linked_worktree");
        }
        atomic_json(&record, &binding).unwrap();
        if history_copies {
            // An alias of the surviving parent exercises missing-root identity
            // lookup on Unix, just as verbatim paths do on Windows.
            #[cfg(unix)]
            let memo_root = {
                let alias = temp.path().join("memo-parent");
                std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
                alias.join(root.file_name().unwrap())
            };
            #[cfg(not(unix))]
            let memo_root = root.clone();
            atomic_json(
                &temp.path().join("cache-keys.json"),
                &serde_json::json!({(memo_root.to_string_lossy().as_ref()): {
                    "key": scope, "recorded_at_ms": last_bound - 1000
                }}),
            )
            .unwrap();
            // Use a real exited PID: Windows tasklist rejects u32::MAX, which
            // correctly leaves that synthetic owner's liveness unknown.
            let mut command = std::process::Command::new("git");
            crate::test_env::apply_hermetic_git_env(&mut command);
            let mut child = command
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let owner_pid = child.id();
            assert!(child.wait().unwrap().success());
            crate::artifact_owner::write_synthetic_manifest_for_test(
                temp.path(),
                &root,
                &scope,
                &scope,
                owner_pid,
                last_bound + 1000,
            );
        }
        (temp, root, scope)
    }

    #[test]
    fn storage_retention_linked_worktree_past_margin_with_history_copies_is_reclaimed() {
        let (temp, root, scope, path) = deleted_checkout_fixture(true, 11, false, true);
        let history = bindings(temp.path()).unwrap();
        // Each source contributes a scope and an artifact-key copy. Keep all
        // their timestamps, even though they describe the same checkout.
        assert_eq!(
            history[&scope].len(),
            6,
            "scope={scope}, history={history:?}"
        );
        let original: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        let mut clocks: Vec<_> = history[&scope]
            .iter()
            .map(|binding| binding.last_bound_ms)
            .collect();
        clocks.sort_unstable();
        assert_eq!(
            clocks,
            vec![
                original.last_bound_ms - 1000,
                original.last_bound_ms - 1000,
                original.last_bound_ms,
                original.last_bound_ms,
                original.last_bound_ms + 1000,
                original.last_bound_ms + 1000,
            ]
        );
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(!path.exists());
        assert!(missing_root_due(temp.path(), &root, 0));
        let record: serde_json::Value =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        assert_eq!(record["linked_worktree"], true);
    }

    #[test]
    fn storage_retention_linked_worktree_inside_margin_is_kept() {
        let (temp, root, _scope, path) = deleted_checkout_fixture(true, 9, false, false);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        assert!(!missing_root_due(temp.path(), &root, 0));
    }

    #[test]
    fn storage_retention_present_linked_worktree_past_margin_is_kept() {
        let (temp, root, scope) = bound_checkout_fixture(true, 11, false, true);
        let path = cache(temp.path(), "views", &scope);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        assert!(!missing_root_due(temp.path(), &root, 0));
    }

    #[test]
    fn storage_retention_linked_worktree_recent_owner_clock_still_keeps_root() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 11, false, true);
        let owner = temp
            .path()
            .join(format!("artifact-owners/{scope}/owner.json"));
        let mut metadata: serde_json::Value = read_json(&owner).unwrap();
        metadata["heartbeat_at_ms"] = (crate::pins::now_ms() - 9 * 60 * 1000).into();
        atomic_json(&owner, &metadata).unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_main_checkout_25h_is_kept() {
        let (temp, _root, _scope, path) = deleted_checkout_fixture(false, 25 * 60, false, false);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_ambiguous_git_file_does_not_record_linked_topology() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let separate_git = temp.path().join("separate-git");
        fs::create_dir(&separate_git).unwrap();
        for target in [separate_git, temp.path().join("missing-git")] {
            fs::write(root.join(".git"), format!("gitdir: {}\n", target.display())).unwrap();
            record_bind(temp.path(), &root, "0123456789abcdef").unwrap();
            let scope = crate::path_identity::project_scope_key(&root);
            let binding: Binding =
                read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
            assert!(!binding.linked_worktree);
        }
    }

    #[test]
    fn storage_retention_linked_worktree_unflagged_history_copies_keep_seven_days() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 25 * 60, true, true);
        let history = bindings(temp.path()).unwrap();
        assert_eq!(
            history[&scope].len(),
            6,
            "scope={scope}, history={history:?}"
        );
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_legacy_binding_uses_seven_days() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 25 * 60, true, false);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: serde_json::Value = read_json(&record).unwrap();
        binding["last_bound_ms"] = (crate::pins::now_ms() - 8 * 24 * 60 * 60 * 1000).into();
        atomic_json(&record, &binding).unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 1);
        assert!(!path.exists());
    }

    #[test]
    fn storage_retention_linked_worktree_past_margin_keeps_live_reader_and_mount_checks() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 11, false, true);
        let _uuid = uuid_backed_binding(temp.path(), &scope);
        let marker = crate::root_cache::ReadMarker::create(&path, "query").unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        drop(marker);
        let binding =
            disconnect_uuid_backed_binding(temp.path(), &scope, &temp.path().join("unmounted"));
        assert!(!mount_matches(&binding));
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_keeps_a_disconnected_volume() {
        let (temp, root, scope) = fixture();
        let _uuid = uuid_backed_binding(temp.path(), &scope);
        let binding = disconnect_uuid_backed_binding(
            temp.path(),
            &scope,
            &temp.path().join("unmounted-volume"),
        );
        fs::remove_dir(root).unwrap();
        assert!(!mount_matches(&binding));
        let path = cache(temp.path(), "index", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_keeps_pins_and_open_file_sets() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let path = cache(temp.path(), "inspect", &scope);
        let marker = crate::root_cache::ReadMarker::create(&path, "query").unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        drop(marker);
        let db = path.join("open.sqlite");
        let connection =
            crate::db::file_identity::IdentityConnection::open(&db, "retention fixture").unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        drop(connection);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 1);
        assert!(!path.exists());
    }

    fn due_cache_fixture() -> (tempfile::TempDir, PathBuf, String, PathBuf) {
        let (temp, root, scope) = fixture();
        fs::remove_dir(&root).unwrap();
        let path = cache(temp.path(), "views", &scope);
        (temp, root, scope, path)
    }

    #[test]
    fn retention_due_root_keeps_query_pin() {
        let (temp, _, _, path) = due_cache_fixture();
        let _pin = crate::pins::QueryPin::acquire(&path, "old").unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        assert!(report.skipped_protected > 0);
    }

    #[test]
    fn retention_due_root_keeps_writer_lock() {
        let (temp, _, _, path) = due_cache_fixture();
        let _writer =
            crate::fs_lock::try_acquire(&path.join("writer.lease"), Duration::ZERO).unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        assert!(report.skipped_protected > 0);
    }

    #[test]
    fn retention_due_root_keeps_unknown_pin_state() {
        let (temp, _, _, path) = due_cache_fixture();
        fs::create_dir_all(path.join("pins")).unwrap();
        fs::write(path.join("pins/broken.json"), b"not pin metadata").unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        assert!(!report.errors.is_empty());
    }

    #[test]
    fn retention_due_root_rechecks_rebind_before_rename() {
        let (temp, root, scope, path) = due_cache_fixture();
        let (worker, release) = paused_pass(temp.path(), &path, "walk");
        fs::create_dir(&root).unwrap();
        record_bind(temp.path(), &root, &scope).unwrap();
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
    }

    #[test]
    fn retention_reaper_resumes_retired_roots_without_a_pass() {
        let (temp, _, _, path) = due_cache_fixture();
        retire_directory(&path).unwrap();
        let trash = temp.path().join("views/.retention-trash");
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 1);
        assert!(!path.exists());
        // No in-memory queue is needed to discover a rename left by an exit.
        reap_retired(temp.path()).unwrap();
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
    }

    #[test]
    fn retention_due_rename_does_not_wait_for_background_unlink() {
        let (temp, _, _, path) = due_cache_fixture();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reap-root" {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let started = Instant::now();
        let report = run_pass(temp.path(), &|| false);
        let elapsed = started.elapsed();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let retired = fs::read_dir(temp.path().join("views/.retention-trash"))
            .unwrap()
            .count();
        release_tx.send(()).unwrap();
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert_eq!(report.unmeasured_removed_roots, 1);
        assert!(!path.exists());
        assert_eq!(retired, 1, "pass unlinked the root synchronously");
        assert!(
            elapsed < PASS_BUDGET,
            "background unlink consumed the pass budget"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retention_due_root_keeps_symlink_and_reaper_keeps_unknown_trash() {
        let (temp, _, _, path) = due_cache_fixture();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("important"), b"not a cache").unwrap();
        std::os::unix::fs::symlink(&outside, path.join("link")).unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        retire_directory(&path).unwrap();
        reap_retired(temp.path()).unwrap();
        assert_eq!(
            fs::read_dir(temp.path().join("views/.retention-trash"))
                .unwrap()
                .count(),
            1
        );
        assert!(outside.join("important").exists());
    }

    #[test]
    fn storage_retention_unknown_keys_receive_an_observation_grace() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        let path = cache(temp.path(), "symbols", key);
        filetime::set_file_mtime(
            path.join("payload.bin"),
            filetime::FileTime::from_unix_time(0, 0),
        )
        .unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        atomic_json(
            &temp.path().join(format!("retention/unknown/{key}.json")),
            &0_u64,
        )
        .unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 1);
        assert!(!path.exists());
        assert!(
            !temp
                .path()
                .join(format!("retention/unknown/{key}.json"))
                .exists(),
            "deleted cache retained its old observation grace"
        );
    }

    #[test]
    fn storage_retention_observation_restart_child() {
        let Some(storage) = std::env::var_os("AFT_RETENTION_RESTART_STORAGE") else {
            return;
        };
        let storage = PathBuf::from(storage);
        let now: u64 = std::env::var("AFT_RETENTION_RESTART_NOW")
            .unwrap()
            .parse()
            .unwrap();
        let key = "0123456789abcdef";
        let mut report = SweepReport::default();
        let mut due = DueBatch::default();
        candidate(
            &storage,
            &storage.join("symbols"),
            key,
            false,
            &BTreeMap::new(),
            now,
            Instant::now() + Duration::from_secs(5),
            &|| false,
            &mut ViewMembers::default(),
            &mut due,
            &mut report,
        )
        .unwrap();
        due.flush(&storage, now, &|| false, &mut report).unwrap();
        atomic_json(&storage.join(format!("restart-result-{now}.json")), &report).unwrap();
    }

    #[test]
    fn storage_retention_unknown_observation_survives_process_restart() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        // A symbols cache, not a checkout view folder: view folders without
        // history follow the hour-long orphan rule, while every other domain
        // keeps the week-long observation grace this test exercises.
        let view = cache(temp.path(), "symbols", key);
        // An old payload alone must not authorize deletion on first sight.
        // The synthetic clock advances a week without editing the observation
        // file or waiting a week; every observation uses a fresh OS process.
        filetime::set_file_mtime(
            view.join("payload.bin"),
            filetime::FileTime::from_unix_time(0, 0),
        )
        .unwrap();
        let first_seen = 1000_u64;
        let observation = temp.path().join(format!("retention/unknown/{key}.json"));
        for (now, should_remove) in [
            (first_seen, false),
            (first_seen + ROOT_GRACE_MS - 1, false),
            (first_seen + ROOT_GRACE_MS, true),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage_retention::storage_retention_tests::storage_retention_observation_restart_child",
                ])
                .env("AFT_RETENTION_RESTART_STORAGE", temp.path())
                .env("AFT_RETENTION_RESTART_NOW", now.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "retention child failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if should_remove {
                assert!(
                    !observation.exists(),
                    "removed cache kept its old observation"
                );
            } else {
                assert_eq!(
                    read_json::<u64>(&observation).unwrap(),
                    first_seen,
                    "a restarted pass replaced the first observation"
                );
            }
            let result: serde_json::Value =
                read_json(&temp.path().join(format!("restart-result-{now}.json"))).unwrap();
            assert_eq!(result["removed_roots"], usize::from(should_remove));
            assert_eq!(
                view.exists(),
                !should_remove,
                "a fresh process did not honor the original observation grace at {now}"
            );
        }
    }

    #[test]
    fn storage_retention_cancellation_leaves_the_root_untouched() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let path = cache(temp.path(), "symbols", &scope);
        let report = run_pass(temp.path(), &|| true);
        assert!(report.cancelled);
        assert_eq!(report.removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_bounds_enumeration_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        for number in 0..80 {
            cache(temp.path(), "symbols", &format!("{number:016x}"));
        }
        let first = run_pass_entry_bound(temp.path());
        let second = run_pass_entry_bound(temp.path());
        assert_eq!(first.examined, PASS_ENTRIES);
        assert_eq!(second.examined, 16);
        assert!(first.stopped_early);
    }

    #[test]
    fn storage_retention_time_budget_stops_the_walk_before_the_entry_bound() {
        let temp = tempfile::tempdir().unwrap();
        for number in 0..80 {
            cache(temp.path(), "symbols", &format!("{number:016x}"));
        }
        // An exhausted budget stops the pass before its first entry, however
        // many entries remain, and leaves the cursor where it was.
        let stopped = run_pass_with_budget(temp.path(), &|| false, &Instant::now, Duration::ZERO);
        assert_eq!(stopped.examined, 0, "{stopped:?}");
        assert!(stopped.stopped_early, "{stopped:?}");
        let resumed = run_pass_entry_bound(temp.path());
        assert_eq!(resumed.examined, PASS_ENTRIES, "{resumed:?}");
    }

    /// Makes an orphan view look observed, and unchanged, for over an hour.
    fn age_orphan(storage: &Path, view: &Path, key: &str) {
        let two_hours_ago = (crate::pins::now_ms() / 1000) as i64 - 2 * 60 * 60;
        filetime::set_file_mtime(
            view.join("payload.bin"),
            filetime::FileTime::from_unix_time(two_hours_ago, 0),
        )
        .unwrap();
        atomic_json(
            &storage.join(format!("retention/unknown/{key}.json")),
            &(crate::pins::now_ms() - ORPHAN_VIEW_GRACE_MS - 60_000),
        )
        .unwrap();
    }

    #[test]
    fn storage_retention_orphan_view_is_reclaimed_after_its_short_grace() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        let view = cache(temp.path(), "views", key);
        // Expected: the first pass keeps the folder. A folder seen for the
        // first time may belong to a build in progress, so it is counted as
        // recent and its observation clock starts.
        let first = run_pass(temp.path(), &|| false);
        assert_eq!(first.removed_roots, 0, "{first:?}");
        assert_eq!(first.orphan_views.skipped_recent, 1, "{first:?}");
        assert!(view.exists());
        // Expected: once the folder has been observed, and left unchanged, for
        // over an hour, the next pass removes it and reports its bytes.
        age_orphan(temp.path(), &view, key);
        let second = run_pass(temp.path(), &|| false);
        assert_eq!(second.removed_roots, 1, "{second:?}");
        assert_eq!(second.orphan_views.removed_roots, 1, "{second:?}");
        assert!(second.orphan_views.removed_bytes > 0, "{second:?}");
        assert!(!view.exists());
    }

    #[test]
    fn storage_retention_orphan_view_registration_in_progress_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        let view = cache(temp.path(), "views", &scope);
        age_orphan(temp.path(), &view, &scope);
        // The pass loads binding history when it starts, finds none for this
        // folder, and so treats it as an orphan. While the pass is paused in
        // its walk, a registration binds the checkout root. The binding is
        // recorded under the family's artifact key (the key shared by every
        // checkout of one repository), as configure and register_view do.
        // Expected: the recheck under the barrier sees the new binding and
        // keeps the folder.
        let (worker, release) = paused_pass(temp.path(), &view, "walk");
        record_bind(temp.path(), &root, "fedcba9876543210").unwrap();
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(view.exists());
    }

    #[test]
    fn storage_retention_unreadable_registry_keeps_every_orphan_view() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        let view = cache(temp.path(), "views", key);
        age_orphan(temp.path(), &view, key);
        let family = temp.path().join("blobs/v2/fedcba9876543210");
        fs::create_dir_all(&family).unwrap();
        fs::write(
            family.join(crate::views::registry::REGISTRY_FILE),
            b"not a registry database",
        )
        .unwrap();
        // Expected: a registry that cannot be read might list this folder, so
        // the folder is kept and counted as unknown, never as an orphan.
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_unknown, 1, "{report:?}");
        assert_eq!(report.orphan_views.skipped_unknown, 1, "{report:?}");
        assert!(view.exists());
    }

    #[test]
    fn storage_retention_registered_view_without_history_keeps_the_week() {
        let temp = tempfile::tempdir().unwrap();
        let scope = "0123456789abcdef";
        let registry =
            crate::views::registry::FamilyRegistry::open(temp.path(), "fedcba9876543210").unwrap();
        // Registering with a root that does not exist records no binding, so
        // the registry is the only record of this scope.
        drop(
            registry
                .register_view(scope, &temp.path().join("missing"))
                .unwrap(),
        );
        let view = cache(temp.path(), "views", scope);
        age_orphan(temp.path(), &view, scope);
        // Expected: a folder that a registry lists is never an orphan. It keeps
        // the week-long grace, so an hour-old folder is kept and the orphan
        // counts stay at zero.
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.orphan_views.examined, 0, "{report:?}");
        assert!(view.exists());
    }

    #[test]
    fn storage_retention_due_caches_do_not_wait_for_the_lexical_walk() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        // The due key is the checkout's scope: its root is deleted and its
        // binding is past the grace. A lexical batch is the set of
        // PASS_ENTRIES cache names one pass takes in name order across the
        // domains. Create more fresh view folders than one batch holds; their
        // names sort before the due key's symbols cache, so the batch never
        // reaches that cache.
        for number in 0..PASS_ENTRIES + 8 {
            cache(temp.path(), "views", &format!("{number:016x}"));
        }
        let path = cache(temp.path(), "symbols", &scope);
        // Expected: the due-first lane removes the due key's cache in this
        // same pass, without waiting for the batch to reach it.
        let report = run_pass_entry_bound(temp.path());
        assert_eq!(report.due_keys, 1, "{report:?}");
        assert_eq!(report.due_first.removed_roots, 1, "{report:?}");
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(!path.exists());
    }

    #[test]
    fn storage_retention_vanished_view_backlog_drains_in_two_passes() {
        const ROOTS: usize = 600;
        let (temp, root, scope) = fixture();
        let template: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        fs::remove_dir(root).unwrap();
        for number in 0..ROOTS {
            let key = format!("{number:016x}");
            let mut binding = template.clone();
            binding.root = temp.path().join(format!("deleted-{number}"));
            binding.artifact_key = key.clone();
            atomic_json(
                &temp.path().join(format!("retention/roots/{key}.json")),
                &binding,
            )
            .unwrap();
            cache(temp.path(), "views", &key);
            cache(temp.path(), "callgraph", &key);
        }
        let remaining = || {
            (0..ROOTS)
                .filter(|number| temp.path().join(format!("views/{number:016x}")).exists())
                .count()
        };
        let mut passes = 0;
        drain_due_views(|| {
            let report = run_pass_entry_bound(temp.path());
            passes += 1;
            assert_eq!(report.due_views_remaining, remaining(), "{report:?}");
            assert!(report.due_first.examined <= DUE_FIRST_ENTRIES, "{report:?}");
            if passes == 1 {
                assert_eq!(report.due_first.removed_roots, 256, "{report:?}");
                assert_eq!(
                    remaining(),
                    ROOTS - 256 - PASS_ENTRIES,
                    "views must precede other domains"
                );
            }
            assert!(passes <= 2, "vanished views stalled: {report:?}");
            report
        });
        assert_eq!(passes, 2);
        assert_eq!(remaining(), 0);
    }

    /// A drain burst over a large backlog. Every removal rechecks binding
    /// history under the retention barrier, and that history is every binding
    /// record in the storage. The recheck must not reread all of it for each
    /// removed cache: with thousands of records that made each pass cost
    /// seconds of file reads, and a burst runs pass after pass.
    #[test]
    fn storage_retention_due_drain_rereads_history_per_batch_not_per_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const DUE: usize = 600;
        const LIVE: usize = 1400;
        let (temp, root, scope) = fixture();
        let template: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        fs::remove_dir(root).unwrap();
        // Checkouts live outside the storage folder, as they do in production.
        let checkouts = tempfile::tempdir().unwrap();
        for number in 0..DUE + LIVE {
            let key = format!("{number:016x}");
            let mut binding = template.clone();
            binding.artifact_key = key.clone();
            if number < DUE {
                binding.root = checkouts.path().join(format!("deleted-{number}"));
                cache(temp.path(), "views", &key);
                cache(temp.path(), "callgraph", &key);
            } else {
                // Bindings whose checkout still exists: every admission
                // recheck reads them, and retention must never remove them.
                let live = checkouts.path().join(format!("live-{number}"));
                fs::create_dir(&live).unwrap();
                binding.root = live;
            }
            atomic_json(
                &temp.path().join(format!("retention/roots/{key}.json")),
                &binding,
            )
            .unwrap();
        }
        let history_reads = Arc::new(AtomicUsize::new(0));
        let owner_scans = Arc::new(AtomicUsize::new(0));
        let _hook = {
            let history_reads = history_reads.clone();
            let owner_scans = owner_scans.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    "history-read" => {
                        history_reads.fetch_add(1, Ordering::Relaxed);
                    }
                    "owner-scan" => {
                        owner_scans.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }),
            )
        };
        let started = Instant::now();
        let mut passes = Vec::new();
        drain_due_views(|| {
            let reads_before = history_reads.load(Ordering::Relaxed);
            let scans_before = owner_scans.load(Ordering::Relaxed);
            let report = run_pass_entry_bound(temp.path());
            let reads = history_reads.load(Ordering::Relaxed) - reads_before;
            let scans = owner_scans.load(Ordering::Relaxed) - scans_before;
            eprintln!(
                "retention drain pass={} removed_roots={} examined={} due_first_examined={} due_views_remaining={} history_reads={reads} owner_scans={scans} errors={:?} timings_ms={:?}",
                passes.len() + 1,
                report.removed_roots,
                report.examined,
                report.due_first.examined,
                report.due_views_remaining,
                report.errors,
                report.timings_ms
            );
            passes.push((
                report.removed_roots,
                reads,
                scans,
                report.due_views_remaining,
            ));
            report
        });
        let removed: usize = passes.iter().map(|(removed, ..)| removed).sum();
        eprintln!(
            "retention drain due={DUE} live={LIVE} passes={} removed={removed} elapsed_ms={}",
            passes.len(),
            started.elapsed().as_millis()
        );
        assert_eq!(
            passes.last().map(|(.., remaining)| *remaining),
            Some(0),
            "the burst left due views: {passes:?}"
        );
        assert!(removed >= DUE, "the burst removed too little: {passes:?}");
        for (removed, reads, scans, _) in &passes {
            // One read at pass setup, and one per admission batch of removals
            // (each lane flushes its own partial batch, hence the slack).
            let batches = removed.div_ceil(ADMISSION_BATCH) + 2;
            assert!(
                *reads <= 1 + batches,
                "history reread per removal: removed={removed} reads={reads}"
            );
            assert!(
                *scans <= 1 + batches,
                "owners rescanned per removal: removed={removed} scans={scans}"
            );
        }
    }

    #[test]
    fn storage_retention_backlog_burst_stops_without_progress_or_on_cancellation() {
        for cancelled in [false, true] {
            let mut passes = 0;
            drain_due_views(|| {
                passes += 1;
                SweepReport {
                    due_views_remaining: 3,
                    cancelled,
                    ..Default::default()
                }
            });
            assert_eq!(passes, 1);
        }
    }

    #[test]
    fn storage_retention_large_due_backlog_drains_within_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const ROOTS: usize = 256;
        const FILES: usize = 64;
        const MIN_REMOVED: usize = 32;
        let (temp, root, scope) = fixture();
        let template: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        fs::remove_dir(root).unwrap();
        let measurements = Arc::new(AtomicUsize::new(0));
        let walks = Arc::new(AtomicUsize::new(0));
        let cursor_commits = Arc::new(AtomicUsize::new(0));
        let mut hooks = Vec::new();
        for number in 0..ROOTS {
            let key = format!("{number:016x}");
            let mut binding = template.clone();
            binding.root = temp.path().join(format!("deleted-{number}"));
            binding.artifact_key = key.clone();
            atomic_json(
                &temp.path().join(format!("retention/roots/{key}.json")),
                &binding,
            )
            .unwrap();
            let path = cache(temp.path(), "views", &key);
            for file in 0..FILES {
                // File count exercises traversal cost; small payloads also work
                // on filesystems that allocate the full length of an extended file.
                fs::write(path.join(format!("payload-{file}.bin")), b"rebuildable").unwrap();
            }
            let measurements = measurements.clone();
            let walks = walks.clone();
            hooks.push(observe_test_hook(
                &path,
                Arc::new(move |step| {
                    if step == "measure-file" {
                        measurements.fetch_add(1, Ordering::Relaxed);
                    } else if step == "walk" {
                        walks.fetch_add(1, Ordering::Relaxed);
                    }
                }),
            ));
        }
        let commits = cursor_commits.clone();
        hooks.push(observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "cursor-commit" {
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            }),
        ));
        let orphan_key = "ffffffffffffffff";
        let orphan = cache(temp.path(), "views", orphan_key);
        age_orphan(temp.path(), &orphan, orphan_key);
        let run_bounded_pass = || {
            let clock_start = Instant::now();
            let clock_calls = AtomicUsize::new(0);
            let clock = || {
                if clock_calls.fetch_add(1, Ordering::Relaxed) < MIN_REMOVED {
                    clock_start
                } else {
                    // Prioritized due candidates stop at half the pass budget. After
                    // MIN_REMOVED checks, jump past that cutoff but stay before
                    // the unbound-view stage's three-quarter cutoff and the
                    // full-budget cutoff of the general directory traversal.
                    clock_start + NO_TIME_BOUND * 5 / 8
                }
            };
            // Inner safety walks still use wall time. Their generous guard is
            // independent of the logical clock that bounds the candidate count.
            run_pass_with_budget(temp.path(), &|| false, &clock, NO_TIME_BOUND)
        };
        let report = run_bounded_pass();
        let remaining = (0..ROOTS)
            .filter(|number| temp.path().join(format!("views/{number:016x}")).exists())
            .count();
        eprintln!("retention fixture roots={ROOTS} files_per_root={FILES} logical_due_quota={MIN_REMOVED} measured_files={} walks={} cursor_commits={} due_removed={} remaining={} orphan_examined={} errors={:?}", measurements.load(Ordering::Relaxed), walks.load(Ordering::Relaxed), cursor_commits.load(Ordering::Relaxed), report.due_first.removed_roots, remaining, report.orphan_views.examined, report.errors);
        assert_eq!(
            report.due_first.removed_roots, MIN_REMOVED,
            "due backlog did not drain: {report:?}"
        );
        assert_eq!(
            remaining,
            ROOTS - MIN_REMOVED - PASS_ENTRIES,
            "backlog did not shrink"
        );
        assert_eq!(
            report.orphan_views.examined, 1,
            "orphan lane starved: {report:?}"
        );
        assert_eq!(
            measurements.load(Ordering::Relaxed),
            0,
            "due roots were byte-measured"
        );
        assert_eq!(
            cursor_commits.load(Ordering::Relaxed),
            2,
            "cursor was synced per entry"
        );
        let second = run_bounded_pass();
        let second_remaining = (0..ROOTS)
            .filter(|number| temp.path().join(format!("views/{number:016x}")).exists())
            .count();
        eprintln!(
            "retention second pass due_removed={} remaining={second_remaining}",
            second.due_first.removed_roots
        );
        assert_eq!(
            second.due_first.removed_roots, MIN_REMOVED,
            "second pass stalled: {second:?}"
        );
        assert_eq!(
            second_remaining,
            remaining - MIN_REMOVED - PASS_ENTRIES,
            "second pass did not shrink backlog"
        );
        assert_eq!(
            measurements.load(Ordering::Relaxed),
            0,
            "second pass byte-measured due roots"
        );
        assert_eq!(
            cursor_commits.load(Ordering::Relaxed),
            4,
            "second pass did not commit each batch once"
        );
    }

    #[test]
    fn storage_retention_legacy_binding_of_a_present_worktree_is_upgraded() {
        let (temp, root, scope) = bound_checkout_fixture(true, 11, true, false);
        let record_path = temp.path().join(format!("retention/roots/{scope}.json"));
        let legacy: serde_json::Value = read_json(&record_path).unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.upgraded_bindings, 1, "{report:?}");
        let record: serde_json::Value = read_json(&record_path).unwrap();
        assert_eq!(record["linked_worktree"], true);
        assert_eq!(record["last_bound_ms"], legacy["last_bound_ms"]);
        let path = cache(temp.path(), "views", &scope);
        fs::remove_dir_all(&root).unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(!path.exists());
    }

    #[test]
    fn storage_retention_v2_recent_missing_root_survives_repeated_sweeps() {
        let (temp, root, scope) = fixture();
        let registry = crate::views::registry::FamilyRegistry::open(temp.path(), &scope).unwrap();
        let view = registry.register_view(&scope, &root).unwrap();
        let path = view.view_dir().to_path_buf();
        drop(view);
        fs::remove_dir(root).unwrap();
        for _ in 0..3 {
            crate::gc::family::sweep_family(
                &registry,
                None,
                crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
                None,
            )
            .unwrap();
        }
        assert!(registry.member(&scope).unwrap().is_some());
        assert!(path.exists());
    }

    #[test]
    fn v2_mount_mismatch_is_counted_and_keeps_member() {
        let (temp, root, scope) = fixture();
        let registry = crate::views::registry::FamilyRegistry::open(temp.path(), &scope).unwrap();
        let view = registry.register_view(&scope, &root).unwrap();
        let path = view.view_dir().to_path_buf();
        drop(view);
        registry
            .with_barrier(|tx| {
                tx.execute("UPDATE members SET last_bind_ms = 0", [])?;
                Ok(())
            })
            .unwrap();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.last_bound_ms = 0;
        binding.volume = Some((temp.path().join("Volumes/X"), u64::MAX));
        binding.stable_volume = None;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        let report = crate::gc::family::sweep_family(
            &registry,
            None,
            crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
            None,
        )
        .unwrap();
        assert_eq!(report.examined_roots, 1);
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert_eq!(report.skipped_recent, 0);
        assert!(registry.member(&scope).unwrap().is_some());
        assert!(path.exists());
    }
}

fn collect_one_family(
    storage: &Path,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    visit_one_family(storage, "blobs", "families-v1", cancelled, |entry| {
        let family = entry.file_name().to_string_lossy().into_owned();
        if !key_valid(&family) || !entry.file_type()?.is_dir() || cancelled() {
            return Ok(());
        }
        let swept = crate::gc::sweep_bounded(
            crate::gc::SweepRequest {
                storage,
                family: &family,
                view_dir: &storage.join("views/retention-no-view"),
                byte_budget: 0,
                now_ms: report.at_ms,
                references: Default::default(),
            },
            cancelled,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        report.examined_blobs += swept.examined_blobs;
        report.removed_blobs += swept.deleted_blobs;
        report.removed_blob_payload_bytes += swept.deleted_bytes;
        report.blob_reclaim_deferred |= swept.reclaim_deferred;
        report
            .domains
            .entry("blobs".to_owned())
            .or_default()
            .add(&DomainReport {
                examined: swept.examined_blobs,
                removed_bytes: swept.deleted_bytes,
                ..Default::default()
            });
        Ok(())
    })
}

// Forget history only after its checkout-private caches and registry member
// have gone. A shared family still in use retains its own live owner's record.
fn prune_bindings(
    storage: &Path,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    static RECORDS: OnceLock<Mutex<HashMap<PathBuf, ReadDir>>> = OnceLock::new();
    let mut records = RECORDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !records.contains_key(storage) {
        match fs::read_dir(storage.join("retention/roots")) {
            Ok(entries) => {
                records.insert(storage.to_path_buf(), entries);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    let batch = records
        .get_mut(storage)
        .map(|entries| entries.by_ref().take(32).collect::<io::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    if batch.len() < 32 {
        records.remove(storage);
    }
    drop(records);
    for entry in batch {
        if cancelled() {
            break;
        }
        report.examined_bindings += 1;
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let record: serde_json::Value = read_json(&entry.path())?;
        let binding: Binding = serde_json::from_value(record.clone()).map_err(io::Error::other)?;
        if record.get("linked_worktree").is_none() && matches!(binding.root.try_exists(), Ok(true))
        {
            report.upgraded_bindings +=
                usize::from(upgrade_legacy_binding(storage, &entry.path())?);
            continue;
        }
        if !missing_and_old(&binding, report.at_ms) {
            continue;
        }
        let scope = crate::path_identity::project_scope_key(&binding.root);
        if DOMAINS
            .iter()
            .any(|domain| storage.join(domain).join(&scope).exists())
            || storage.join("views/v2").join(&scope).exists()
        {
            continue;
        }
        if crate::root_cache::protected_read_marker_exists(&storage.join("retention"), &scope) {
            continue;
        }
        if key_valid(&binding.artifact_key) {
            let owner_path = storage
                .join("artifact-owners")
                .join(&binding.artifact_key)
                .join("owner.json");
            let family_live =
                read_json::<crate::artifact_owner::ArtifactOwnerManifest>(&owner_path)
                    .is_ok_and(|owner| Path::new(&owner.checkout_path).exists());
            if !family_live
                && DOMAINS
                    .iter()
                    .any(|domain| storage.join(domain).join(&binding.artifact_key).exists())
            {
                continue;
            }
            if let Some(registry) = crate::views::registry::FamilyRegistry::open_existing(
                storage,
                &binding.artifact_key,
            )
            .map_err(|error| io::Error::other(error.to_string()))?
            {
                if registry
                    .members_bounded(HISTORY_ENTRIES)
                    .map_err(|error| io::Error::other(error.to_string()))?
                    .iter()
                    .any(|member| {
                        member.root.as_ref().is_some_and(|root| {
                            crate::root_cache::canonical_root(root) == binding.root
                        })
                    })
                {
                    continue;
                }
            }
        }
        let mut legacy_present = false;
        for (directory, flat) in domain_paths(storage)? {
            if !flat {
                continue;
            }
            if bounded_dirs(&directory, TREE_ENTRIES)?.iter().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(&format!("{scope}."))
                    || (key_valid(&binding.artifact_key)
                        && name.starts_with(&format!("{}.", binding.artifact_key)))
            }) {
                legacy_present = true;
                break;
            }
        }
        if legacy_present {
            continue;
        }
        let _barrier = barrier(storage)?;
        // A rebind can replace the record while the cache inventory is being
        // read. Never erase that newer observation or its root's history.
        if read_json::<Binding>(&entry.path())? != binding
            || DOMAINS.iter().any(|domain| {
                storage.join(domain).join(&scope).exists()
                    || storage.join(domain).join(&binding.artifact_key).exists()
            })
            || storage.join("views/v2").join(&scope).exists()
            || crate::root_cache::protected_read_marker_exists(&storage.join("retention"), &scope)
        {
            continue;
        }
        fs::remove_file(entry.path())?;
        report.pruned_bindings += 1;
    }
    Ok(())
}

/// A record written before linked-worktree topology was recorded falls back
/// to the week-long grace, because the topology can no longer be proven once
/// the checkout is gone. While the checkout still exists, record the verified
/// topology in place. The bind clock and volume identity stay as recorded.
fn upgrade_legacy_binding(storage: &Path, path: &Path) -> io::Result<bool> {
    let _barrier = barrier(storage)?;
    let record: serde_json::Value = read_json(path)?;
    if record.get("linked_worktree").is_some() {
        return Ok(false);
    }
    let mut binding: Binding = serde_json::from_value(record).map_err(io::Error::other)?;
    if !matches!(binding.root.try_exists(), Ok(true)) {
        return Ok(false);
    }
    binding.linked_worktree = linked_worktree_at_bind(&binding.root);
    atomic_json(path, &binding)?;
    Ok(true)
}

pub(crate) fn schedule(
    storage: PathBuf,
    admission: SubcLifecycleAdmission,
    generation: Arc<AtomicU64>,
    expected: u64,
) {
    if !admission.is_current(&generation, expected) {
        return;
    }
    if !startup_sweeps_ready(&storage) {
        return;
    }
    {
        let mut states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(storage.clone()).or_default();
        if state.running || state.last_run.is_some_and(|last| last.elapsed() < INTERVAL) {
            return;
        }
        state.running = true;
        state.last_run = Some(Instant::now());
    }
    let key = storage.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("aft-storage-retention".into())
        .spawn(move || {
            scheduled_run(&storage, || {
                drain_due_views(|| {
                    let report =
                        run_pass(&storage, &|| !admission.is_current(&generation, expected));
                    crate::slog_info!(
                        "storage retention root={} report={}",
                        storage.display(),
                        serde_json::to_string(&report).unwrap_or_default()
                    );
                    report
                });
            });
        })
    {
        drop(RunningReset {
            storage: &key,
            completed: false,
        });
        crate::slog_warn!("storage retention worker could not start: {error}");
    }
}

/// Each pass keeps its own entry and time bounds. A shrinking vanished-view
/// backlog does not wait ten minutes between passes; protected or unreadable
/// views stop the burst rather than spinning on an uncollectable directory.
fn drain_due_views(mut pass: impl FnMut() -> SweepReport) {
    loop {
        let report = pass();
        if report.cancelled
            || report.due_views_remaining == 0
            || report
                .domains
                .get("views")
                .is_none_or(|views| views.unmeasured_removed_roots == 0)
        {
            break;
        }
    }
}

fn scheduled_run(storage: &Path, run: impl FnOnce()) {
    let mut reset = RunningReset {
        storage,
        completed: false,
    };
    run();
    reset.completed = true;
}

struct RunningReset<'a> {
    storage: &'a Path,
    completed: bool,
}

impl Drop for RunningReset<'_> {
    fn drop(&mut self) {
        let mut states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = states.get_mut(self.storage) {
            state.running = false;
            if !self.completed {
                state.last_run = None;
            }
        }
    }
}

fn clear_unknown_observation(storage: &Path, key: &str) -> io::Result<()> {
    if !key_valid(key) {
        return Ok(());
    }
    let path = storage
        .join("retention/unknown")
        .join(format!("{key}.json"));
    match fs::remove_file(&path) {
        Ok(()) => crate::durability::sync_dir(path.parent().unwrap()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn cache_key_present(storage: &Path, key: &str, deadline: Instant) -> io::Result<bool> {
    for (directory, flat) in domain_paths(storage)? {
        if !flat {
            if directory.join(key).try_exists()? {
                return Ok(true);
            }
        } else if directory.try_exists()? {
            let prefix = format!("{key}.");
            for (path, _) in tree_files(&directory, deadline)? {
                if path.parent() == Some(directory.as_path())
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
