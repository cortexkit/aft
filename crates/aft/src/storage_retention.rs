//! Durable checkout retention, independent of the roots an actor has opened.
//!
//! A missing path is necessary but not sufficient for deletion: binding history
//! must also be old, and all writers, pins, readers and local SQLite handles must
//! have gone. Unknown keys receive a fresh observation grace instead of treating
//! an old payload mtime as proof that a root has not recently been bound.
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, ReadDir};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicU64, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::context::SubcLifecycleAdmission;

pub(crate) const ROOT_GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const INTERVAL: Duration = Duration::from_secs(10 * 60);
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
    pub skipped_protected: usize,
    pub stopped_early: bool,
    pub cancelled: bool,
    pub errors: Vec<String>,
}

#[derive(Default)]
struct State {
    last_run: Option<Instant>,
    running: bool,
    domain: usize,
    entries: Option<ReadDir>,
    report: Option<SweepReport>,
}

fn states() -> &'static Mutex<HashMap<PathBuf, State>> {
    static STATES: OnceLock<Mutex<HashMap<PathBuf, State>>> = OnceLock::new();
    STATES.get_or_init(Default::default)
}

pub(crate) fn snapshot(storage: &Path) -> Option<SweepReport> {
    states().lock().ok()?.get(storage)?.report.clone()
}

#[cfg(test)]
type TestHook = Arc<dyn Fn(&str) + Send + Sync>;

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
    let state = states.get_mut(storage).expect("scheduled retention state");
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
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| io::Error::other("missing retention parent"))?,
    )?;
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec(value).map_err(io::Error::other)?,
    )?;
    crate::fs_lock::rename_over(&temporary, path)
}

/// Called before cache acquisition, on every configure, even a same-key rebind.
pub(crate) fn record_bind(storage: &Path, root: &Path, artifact_key: &str) -> io::Result<()> {
    let _barrier = barrier(storage)?;
    let root = fs::canonicalize(root)?;
    let scope = crate::path_identity::project_scope_key(&root);
    let binding = Binding {
        volume: volume_anchor(&root),
        root,
        artifact_key: artifact_key.to_owned(),
        last_bound_ms: crate::pins::now_ms(),
    };
    atomic_json(
        &storage
            .join("retention/roots")
            .join(format!("{scope}.json")),
        &binding,
    )
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

fn volume_anchor(root: &Path) -> Option<(PathBuf, u64)> {
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

fn missing_and_old(binding: &Binding, now: u64) -> bool {
    if now.saturating_sub(binding.last_bound_ms) < ROOT_GRACE_MS {
        return false;
    }
    if !matches!(binding.root.try_exists(), Ok(false)) {
        return false;
    }
    match &binding.volume {
        Some((anchor, dev)) => device(anchor) == Some(*dev),
        // Old memo records have no mount identity. Never infer a disappeared
        // external/network mount is a deleted checkout from its path alone.
        None => binding
            .root
            .to_string_lossy()
            .contains("/cortexkit/alfonso/worktrees/"),
    }
}

pub(crate) fn missing_root_due(storage: &Path, root: &Path, last_bind_ms: u64) -> bool {
    let root = crate::root_cache::canonical_root(root);
    let scope = crate::path_identity::project_scope_key(&root);
    match read_json::<Binding>(&storage.join(format!("retention/roots/{scope}.json"))) {
        Ok(mut binding) => {
            binding.last_bound_ms = binding.last_bound_ms.max(last_bind_ms);
            missing_and_old(&binding, crate::pins::now_ms())
        }
        Err(_) => missing_and_old(
            &Binding {
                root,
                artifact_key: String::new(),
                last_bound_ms: last_bind_ms,
                volume: None,
            },
            crate::pins::now_ms(),
        ),
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
            };
            result
                .entry(crate::path_identity::project_scope_key(&binding.root))
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

/// Keep the directory iterator itself between passes. No whole-store collect,
/// sort or offset skip precedes the limit, and old roots cannot starve behind a
/// prefix of live ones. Iterators are dropped on cancellation or enumeration end.
fn next_batch(
    storage: &Path,
    domains: &[(PathBuf, bool)],
) -> io::Result<Vec<(PathBuf, bool, fs::DirEntry)>> {
    let mut states = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states.entry(storage.to_path_buf()).or_default();
    let mut result = Vec::new();
    let mut visited = 0;
    while result.len() < PASS_ENTRIES && visited < domains.len() {
        state.domain %= domains.len();
        let (directory, flat) = &domains[state.domain];
        if state.entries.is_none() {
            match fs::read_dir(directory) {
                Ok(entries) => state.entries = Some(entries),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    state.domain += 1;
                    visited += 1;
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        let Some(entries) = state.entries.as_mut() else {
            continue;
        };
        let mut exhausted = false;
        for _ in 0..(PASS_ENTRIES - result.len()) {
            match entries.next() {
                Some(entry) => result.push((directory.clone(), *flat, entry?)),
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        if exhausted {
            state.entries = None;
            state.domain += 1;
            visited += 1;
        }
    }
    Ok(result)
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
    #[cfg(test)]
    test_hook(cache, "walk");
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(cache)?;
    let mut pending = vec![cache.to_path_buf()];
    let mut result = Vec::new();
    let mut examined = 0;
    while let Some(directory) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(io::Error::other("retention deadline reached"));
        }
        for entry in bounded_dirs(&directory, TREE_ENTRIES.saturating_sub(examined))? {
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
            } else {
                result.push((entry.path(), entry.metadata()?.len()));
            }
        }
    }
    Ok(result)
}

pub(crate) fn remove_directory_if_closed(cache: &Path) -> io::Result<()> {
    tree_files(cache, Instant::now() + PASS_BUDGET)?;
    let _files = crate::db::file_identity::filesystem_guard();
    if crate::db::file_identity::has_open_connections_under(cache) {
        return Err(io::Error::other("open SQLite cache retained"));
    }
    fs::remove_dir_all(cache)
}

fn eligible(
    storage: &Path,
    key: &str,
    history: &BTreeMap<String, Vec<Binding>>,
    now: u64,
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
        report.skipped_recent += 1;
        return Ok(false);
    }
    let path = storage
        .join("retention/unknown")
        .join(format!("{key}.json"));
    let first_seen = match read_json::<u64>(&path) {
        Ok(first_seen) => first_seen,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            atomic_json(&path, &now)?;
            now
        }
        Err(error) => return Err(error),
    };
    if now.saturating_sub(first_seen) >= ROOT_GRACE_MS {
        return Ok(true);
    }
    report.skipped_recent += 1;
    Ok(false)
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
    report: &mut SweepReport,
) -> io::Result<()> {
    let cache = if flat {
        directory.to_path_buf()
    } else {
        directory.join(key)
    };
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
    if !eligible(storage, key, history, now, report)? {
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
                .is_ok_and(|mtime| {
                    std::time::SystemTime::now()
                        .duration_since(mtime)
                        .unwrap_or_default()
                        < Duration::from_millis(ROOT_GRACE_MS)
                })
        })
    {
        report.skipped_recent += 1;
        return Ok(());
    }
    // Walks run without either admission or SQLite's process-wide open gate.
    // Recheck protection and binding history under admission immediately before
    // deletion, since a configure or query may have arrived during the walks.
    let _barrier = barrier(storage)?;
    let current_history = bindings(storage)?;
    if !eligible(storage, key, &current_history, now, report)?
        || crate::root_cache::live_scope_keys_for_storage(storage).contains(key)
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
        fs::remove_dir_all(&cache)?;
        report.removed_bytes += after_views
            .iter()
            .map(|(_, bytes)| bytes)
            .sum::<u64>()
            .min(new_bytes);
    }
    report.removed_roots += 1;
    drop(files_guard);
    drop(leases);
    Ok(())
}

pub(crate) fn run_pass(storage: &Path, cancelled: &dyn Fn() -> bool) -> SweepReport {
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
        let deadline = Instant::now() + PASS_BUDGET;
        let batch = next_batch(storage, &domains)?;
        report.stopped_early = batch.len() == PASS_ENTRIES;
        for (directory, flat, entry) in batch {
            if cancelled() {
                report.cancelled = true;
                break;
            }
            if Instant::now() >= deadline {
                report.stopped_early = true;
                break;
            }
            report.examined += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let key = if flat {
                name.split('.').next().unwrap_or_default()
            } else {
                &name
            };
            if !key_valid(key) || (!flat && !entry.file_type()?.is_dir()) {
                continue;
            }
            if flat && !(name.ends_with(".current") || name == format!("{key}.sqlite")) {
                continue;
            }
            if let Err(error) = candidate(
                storage,
                &directory,
                key,
                flat,
                &history,
                report.at_ms,
                deadline,
                cancelled,
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
        Ok(())
    })();
    if let Err(error) = result {
        report.errors.push(error.to_string());
    }
    // Blob stores have their own mark-before-sweep barrier. Release the root
    // barrier before entering it; otherwise pin admission would deadlock.
    if !report.cancelled && !cancelled() {
        if let Err(error) = collect_one_family(storage, cancelled, &mut report) {
            report.errors.push(error.to_string());
        }
        if let Err(error) = collect_one_v2_family(storage, cancelled, &mut report) {
            report.errors.push(error.to_string());
        }
    }
    report.cancelled |= cancelled();
    let mut states = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states.entry(storage.to_path_buf()).or_default();
    if report.cancelled {
        state.entries = None;
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
    static FAMILIES: OnceLock<Mutex<HashMap<PathBuf, ReadDir>>> = OnceLock::new();
    let mut families = FAMILIES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !families.contains_key(storage) {
        match fs::read_dir(storage.join("blobs/v2")) {
            Ok(entries) => {
                families.insert(storage.to_path_buf(), entries);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    let next = families.get_mut(storage).and_then(Iterator::next);
    if next.is_none() {
        families.remove(storage);
    }
    drop(families);
    let Some(entry) = next else { return Ok(()) };
    let entry = entry?;
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
    report.removed_blobs += swept.deleted_blobs;
    report.removed_blob_payload_bytes += swept.deleted_bytes;
    report.blob_reclaim_deferred |= swept.reclaim_deferred;
    report.stopped_early |= swept.stopped_early;
    Ok(())
}

#[cfg(test)]
mod storage_retention_tests {
    use super::*;

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
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 6, "{report:?}");
        assert_eq!(report.removed_bytes, 6 * 11);
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

    #[test]
    fn storage_retention_keeps_a_disconnected_volume() {
        let (temp, root, scope) = fixture();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.volume = Some((temp.path().join("unmounted-volume"), 42));
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        let path = cache(temp.path(), "index", &scope);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
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
        let first = run_pass(temp.path(), &|| false);
        let second = run_pass(temp.path(), &|| false);
        assert_eq!(first.examined, PASS_ENTRIES);
        assert_eq!(second.examined, 16);
        assert!(first.stopped_early);
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
}

fn collect_one_family(
    storage: &Path,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    // One family per pass, with a retained iterator rather than a sorted scan.
    static FAMILIES: OnceLock<Mutex<HashMap<PathBuf, ReadDir>>> = OnceLock::new();
    let families = FAMILIES.get_or_init(Default::default);
    let mut families = families
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !families.contains_key(storage) {
        match fs::read_dir(storage.join("blobs")) {
            Ok(entries) => {
                families.insert(storage.to_path_buf(), entries);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    let next = families.get_mut(storage).and_then(Iterator::next);
    if next.is_none() {
        families.remove(storage);
    }
    drop(families);
    let Some(entry) = next else { return Ok(()) };
    let entry = entry?;
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
    Ok(())
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
        let binding: Binding = read_json(&entry.path())?;
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

pub(crate) fn schedule(
    storage: PathBuf,
    admission: SubcLifecycleAdmission,
    generation: Arc<AtomicU64>,
    expected: u64,
) {
    if !admission.is_current(&generation, expected) {
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
            let report = run_pass(&storage, &|| !admission.is_current(&generation, expected));
            crate::slog_info!(
                "storage retention root={} report={}",
                storage.display(),
                serde_json::to_string(&report).unwrap_or_default()
            );
            if let Ok(mut states) = states().lock() {
                if let Some(state) = states.get_mut(&storage) {
                    state.running = false;
                }
            }
        })
    {
        if let Ok(mut states) = states().lock() {
            if let Some(state) = states.get_mut(&key) {
                state.running = false;
                state.last_run = None;
            }
        }
        crate::slog_warn!("storage retention worker could not start: {error}");
    }
}
