//! Reader markers and process-local root residency for cache cleanup.

use super::{canonical_root, pin_cache_path};
use crate::fs_lock;
use crate::process_identity::process_start_time_ms;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(test)]
use std::fs;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ReadMarkerMetadata {
    pub pid: u32,
    pub hostname: String,
    pub created_at_ms: u64,
    /// Optional acquisition nonce. Ordinary readers leave it out, so their
    /// serialized marker records are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

#[derive(Debug)]
pub struct ReadMarker {
    path: PathBuf,
    generation: crate::pinned_dir::PinnedDir,
    name: std::ffi::OsString,
    metadata: ReadMarkerMetadata,
    last_touched_at_ms: AtomicU64,
}

impl ReadMarker {
    pub fn create(cache_dir: &Path, generation_label: &str) -> io::Result<Self> {
        Self::create_inner(cache_dir, generation_label, None)
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
        crate::private_storage::open_dir(cache_dir, &dir).map_err(|error| {
            crate::io_accounting::at("creating reader marker directory", &dir, error)
        })?;
        let seq = MARKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{}.{}.{}.{}.json",
            metadata.pid,
            sanitize_marker_component(&metadata.hostname),
            metadata.created_at_ms,
            seq
        ));
        let generation = match pin_cache_path(&dir).map_err(|error| {
            crate::io_accounting::at("pinning reader marker directory", &dir, error)
        })? {
            Some(access) => access.directory(false),
            None => crate::pinned_dir::PinnedDir::repair_open_path(&dir),
        }
        .map_err(|error| {
            crate::io_accounting::at("opening reader marker directory", &dir, error)
        })?;
        let name = path.file_name().unwrap().to_os_string();
        write_marker_file_pinned(&generation, &name, &metadata)
            .map_err(|error| crate::io_accounting::at("writing reader marker", &path, error))?;
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
        write_marker_file_pinned(&self.generation, &self.name, &self.metadata).map_err(
            |error| crate::io_accounting::at("writing reader marker", &self.path, error),
        )?;
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

fn read_marker_dir(cache_dir: &Path, generation_label: &str) -> PathBuf {
    cache_dir.join("readers").join(generation_label)
}

/// Sweep generation markers through pinned handles without following links.
#[cfg(test)]
pub(crate) fn sweep_all_read_markers(cache_dir: &Path) -> ReadMarkerSweep {
    let result = crate::pinned_dir::PinnedDir::open(cache_dir).and_then(|dir| {
        sweep_all_read_markers_pinned(&dir, &mut crate::pinned_dir::RepairBudget::new(), true)
    });
    result.unwrap_or(ReadMarkerSweep {
        protected: true,
        removed_stale: 0,
    })
}

pub(crate) fn sweep_all_read_markers_pinned(
    cache: &crate::pinned_dir::PinnedDir,
    budget: &mut crate::pinned_dir::RepairBudget,
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
    generation: &crate::pinned_dir::PinnedDir,
    budget: &mut crate::pinned_dir::RepairBudget,
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
    generation: &crate::pinned_dir::PinnedDir,
    budget: &mut crate::pinned_dir::RepairBudget,
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
    use crate::pinned_dir::{PinnedDir, RepairBudget};
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

#[cfg(test)]
fn protected_read_marker_exists(cache_dir: &Path, generation_label: &str) -> bool {
    read_marker_protection(cache_dir, generation_label, false).protected
}

#[cfg(test)]
pub(crate) fn sweep_read_markers(cache_dir: &Path, generation_label: &str) -> ReadMarkerSweep {
    read_marker_protection(cache_dir, generation_label, true)
}

// Test hook counting hostname resolutions on this thread. Reset before a
// marker pass and read after it to prove the pass resolved the hostname
// once instead of once per generation.
#[cfg(test)]
thread_local! {
    static HOSTNAME_RESOLUTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
        let cache = crate::pinned_dir::PinnedDir::open(cache_dir)?;
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
            &mut crate::pinned_dir::RepairBudget::new(),
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
    let parent = crate::pinned_dir::PinnedDir::repair_open_path(
        path.parent()
            .ok_or_else(|| io::Error::other("marker has no parent"))?,
    )?;
    write_marker_file_pinned(&parent, path.file_name().unwrap(), metadata)
}

fn write_marker_file_pinned(
    generation: &crate::pinned_dir::PinnedDir,
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

fn now_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
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
    use crate::process_identity::{
        clear_process_start_time_for_test, set_process_start_time_for_test,
    };
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
}

#[cfg(test)]
mod residency_tests {
    use super::*;

    #[test]
    fn live_scope_marker_is_held_until_its_last_actor_leaves() {
        let storage = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let key = crate::path_identity::project_scope_key(project.path());
        register_live_scope(storage.path(), project.path());
        register_live_scope(storage.path(), project.path());
        let directory = read_marker_dir(&storage.path().join("retention"), &key);
        let names = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 1, "actors sharing a root share its marker");
        unregister_live_scope(storage.path(), project.path());
        assert!(names[0].exists(), "the other actor still holds the root");
        unregister_live_scope(storage.path(), project.path());
        assert!(!names[0].exists(), "the last actor releases the marker");
    }
}
