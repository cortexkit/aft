//! Repository identities and artifact keys shared by all index-store callers.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::path_identity::lexical_normalize as normalize_path;

const ARTIFACT_CACHE_KEY_MEMO_FILE: &str = "cache-keys.json";
const ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const ARTIFACT_CACHE_KEY_MEMO_READ_REFRESH_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const LEGACY_IDENTITY_MEMO_MAX_AGE: Duration = Duration::from_secs(10 * 60);
const LEGACY_IDENTITY_MEMO_MAX_STORAGE_ROOTS: usize = 4;
static ARTIFACT_CACHE_KEY_MEMO_STATE: OnceLock<Mutex<ArtifactCacheKeyMemoState>> = OnceLock::new();

#[cfg(test)]
type RootCommitProbeOverride =
    Arc<dyn Fn(&Path) -> Option<RootCommitProbe> + Send + Sync + 'static>;

#[cfg(test)]
static GIT_ROOT_COMMIT_PROBE_OVERRIDE: OnceLock<Mutex<Option<RootCommitProbeOverride>>> =
    OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArtifactCacheKeyMemoEntry {
    key: String,
    git_root_commit: String,
    recorded_at_ms: u64,
}

#[derive(Default)]
struct ArtifactCacheKeyMemoState {
    by_storage_root: BTreeMap<PathBuf, BTreeMap<String, ArtifactCacheKeyMemoEntry>>,
}

#[cfg(test)]
type AfterLegacyMemoRename = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! {
    static LEGACY_MEMO_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LEGACY_MEMO_CANONICALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static AFTER_LEGACY_MEMO_RENAME: std::cell::RefCell<Option<AfterLegacyMemoRename>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ArtifactCacheKeyFailure {
    IdentityUnavailable,
    GitUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactCacheKeyProbeError {
    root: PathBuf,
    detail: String,
    failure: ArtifactCacheKeyFailure,
}

impl ArtifactCacheKeyProbeError {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub(crate) fn provisional_git_reason(&self) -> Option<String> {
        if self.failure != ArtifactCacheKeyFailure::GitUnavailable {
            return None;
        }
        let mut reason = format!(
            "{GIT_IDENTITY_UNAVAILABLE_REASON}: {}; indexes and views stay off until this folder's next bind",
            self.detail
        );
        if self.detail.contains("dubious ownership") {
            let repository = crate::root_identity::nearest_git_marker(&self.root)
                .ok()
                .flatten()
                .unwrap_or_else(|| self.root.clone());
            reason.push_str(&format!(
                "; if you trust this repository, run git config --global --add safe.directory '{}'",
                repository.to_string_lossy().replace('\'', "'\\''")
            ));
        }
        Some(reason)
    }
}

impl std::fmt::Display for ArtifactCacheKeyProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "artifact cache key probe failed for {}: {}",
            self.root.display(),
            self.detail
        )
    }
}

impl std::error::Error for ArtifactCacheKeyProbeError {}

pub fn artifact_cache_key(project_root: &Path) -> String {
    let key = match repo_root_commit_with_retry(project_root) {
        RootCommitResolution::Commit(root_commit) => {
            session_artifact_key_from_git_identity(project_root, &root_commit)
        }
        RootCommitResolution::NotARepo => artifact_path_identity_key(project_root),
        RootCommitResolution::Failed(detail) => {
            crate::slog_warn!(
                "artifact cache key: git root-commit probe failed after retries ({}); \
                 falling back to path identity for {}",
                detail,
                project_root.display()
            );
            artifact_path_identity_key(project_root)
        }
        RootCommitResolution::Cancelled => artifact_path_identity_key(project_root),
    };
    record_derived_cache_key(project_root, &key);
    key
}

/// Reuse the artifact key registered during configure when one exists; otherwise
/// derive a key from Git at `project_root`. This fallback is for standalone build
/// APIs whose callers have not configured a project or supplied a storage path.
pub(crate) fn artifact_cache_key_for_build(project_root: &Path) -> String {
    crate::root_cache::configured_artifact_key(project_root)
        .unwrap_or_else(|| artifact_cache_key(project_root))
}

pub fn artifact_cache_key_with_memo(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
) -> Result<String, ArtifactCacheKeyProbeError> {
    artifact_cache_key_with_memo_profiled(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        &mut ArtifactCacheKeyTimings::default(),
    )
}

/// Resolve a reader's key from the same identity records as a session bind,
/// without creating or refreshing identity metadata. A busy legacy database
/// leaves the identity unresolved rather than publishing a provisional key.
pub(crate) fn artifact_cache_key_read_only(
    project_root: &Path,
    storage_root: &Path,
) -> Result<String, ArtifactCacheKeyProbeError> {
    resolve_root_identity(
        project_root,
        project_root,
        storage_root,
        None,
        &mut ArtifactCacheKeyTimings::default(),
        false,
    )
    .map(|identity| identity.key(project_root, false))
}

/// Wall times overlap: memo_hold includes load, pruning and writes, while
/// rev_list includes child execution and waiting for the child to exit.
#[derive(Debug, Default)]
pub(crate) struct ArtifactCacheKeyTimings {
    pub(crate) git_usable: Duration,
    pub(crate) rev_list: Duration,
    /// Git processes actually started, including unborn-branch verification.
    pub(crate) git_spawns: usize,
    /// Full history traversals, excluding cheap HEAD verification on reuse.
    pub(crate) root_walks: usize,
    pub(crate) retry_wait: Duration,
    pub(crate) memo_wait: Duration,
    pub(crate) memo_hold: Duration,
    pub(crate) memo_load: Duration,
    pub(crate) memo_prune: Duration,
    pub(crate) memo_write: Duration,
    pub(crate) legacy_memo: Duration,
    pub(crate) context_memo_wait: Duration,
    pub(crate) context_cache_hit: bool,
}

fn time_cache_key_work<T>(elapsed: &mut Duration, run: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = run();
    *elapsed += started.elapsed();
    result
}

pub(crate) fn artifact_cache_key_with_memo_profiled(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
) -> Result<String, ArtifactCacheKeyProbeError> {
    let identity = resolve_root_identity(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        true,
    )?;
    let key = identity.key(probe_root, false);
    record_derived_cache_key(memo_root, &key);
    Ok(key)
}

/// The artifact key a session bind uses, or `None` while the root's identity
/// is provisional (see [`resolve_root_identity_for_bind`]). A provisional key
/// is not recorded anywhere, so the next bind derives the key again.
pub(crate) fn artifact_cache_key_for_bind_profiled(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
) -> Result<Option<String>, ArtifactCacheKeyProbeError> {
    let bound = resolve_root_identity_for_bind(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        true,
        true,
    )?;
    if bound.provisional {
        return Ok(None);
    }
    let key = bound.identity.key(probe_root, false);
    record_derived_cache_key(memo_root, &key);
    Ok(Some(key))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct LegacyMemoStamp {
    identity: crate::file_identity::FileIdentity,
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl LegacyMemoStamp {
    fn new(metadata: &fs::Metadata, identity: crate::file_identity::FileIdentity) -> Option<Self> {
        if identity == crate::file_identity::FileIdentity::Unavailable {
            return None;
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            identity,
            len: metadata.len(),
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    fn at(path: &Path) -> Option<Self> {
        let (metadata, identity) = crate::file_identity::metadata(path).ok()?;
        Self::new(&metadata, identity)
    }

    fn of_file(file: &File) -> Option<Self> {
        Self::new(
            &file.metadata().ok()?,
            crate::file_identity::FileIdentity::of_file(file),
        )
    }
}

struct LegacyMemoIndex {
    stamp: Option<LegacyMemoStamp>,
    built_at: Instant,
    entries: Vec<(String, ArtifactCacheKeyMemoEntry, PathBuf)>,
    aliases: BTreeMap<PathBuf, Vec<usize>>,
}

fn canonical_legacy_memo_spelling(spelling: &str) -> PathBuf {
    #[cfg(test)]
    LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.set(count.get() + 1));
    crate::root_identity::canonical_root(Path::new(spelling))
}

impl LegacyMemoIndex {
    fn build(
        entries: BTreeMap<String, ArtifactCacheKeyMemoEntry>,
        stamp: Option<LegacyMemoStamp>,
    ) -> Self {
        #[cfg(test)]
        LEGACY_MEMO_BUILDS.with(|count| count.set(count.get() + 1));
        let mut aliases: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        let entries = entries
            .into_iter()
            .enumerate()
            .map(|(index, (spelling, entry))| {
                let canonical = canonical_legacy_memo_spelling(&spelling);
                aliases.entry(canonical.clone()).or_default().push(index);
                (spelling, entry, canonical)
            })
            .collect();
        Self {
            stamp,
            built_at: Instant::now(),
            entries,
            aliases,
        }
    }

    fn apply_write(
        &mut self,
        entries: &BTreeMap<String, ArtifactCacheKeyMemoEntry>,
        stamp: LegacyMemoStamp,
    ) {
        let mut previous = std::mem::take(&mut self.entries).into_iter().peekable();
        let mut aliases: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        let mut updated = Vec::with_capacity(entries.len());
        for (spelling, entry) in entries {
            while previous.peek().is_some_and(|(old, _, _)| old < spelling) {
                previous.next();
            }
            let old = previous
                .peek()
                .is_some_and(|(old, _, _)| old == spelling)
                .then(|| previous.next().unwrap());
            let canonical = match old {
                Some((_, old, canonical))
                    if old.key == entry.key
                        && old.git_root_commit == entry.git_root_commit
                        && old.recorded_at_ms == entry.recorded_at_ms =>
                {
                    canonical
                }
                _ => canonical_legacy_memo_spelling(spelling),
            };
            aliases
                .entry(canonical.clone())
                .or_default()
                .push(updated.len());
            updated.push((spelling.clone(), entry.clone(), canonical));
        }
        // Recreate bucket membership in spelling order after removals/changes.
        // Lookup folds these current entries, so conflict and validation results
        // are recomputed, not retained from an earlier version of a bucket.
        self.entries = updated;
        self.aliases = aliases;
        self.stamp = Some(stamp);
        // Keep built_at: writes must not postpone discovery of new aliases among
        // unchanged spellings by extending their ten-minute lifetime.
    }

    fn lookup(
        &self,
        root: &Path,
        raw_key: &str,
    ) -> std::io::Result<Option<ArtifactCacheKeyMemoEntry>> {
        let canonical = crate::root_identity::canonical_root(root);
        let mut candidates = self.aliases.get(&canonical).cloned().unwrap_or_default();
        if let Ok(raw) = self
            .entries
            .binary_search_by(|(spelling, _, _)| spelling.as_str().cmp(raw_key))
        {
            if let Err(position) = candidates.binary_search(&raw) {
                candidates.insert(position, raw);
            }
        }
        let mut found: Option<&ArtifactCacheKeyMemoEntry> = None;
        for index in candidates {
            let (spelling, entry, indexed_root) = &self.entries[index];
            let current = crate::root_identity::canonical_root(Path::new(spelling));
            // Keep the exact-spelling shortcut while its path mapping is
            // unchanged, but not after it moves away from the query root.
            if current != canonical && !(spelling == raw_key && current == *indexed_root) {
                continue;
            }
            if !artifact_key_looks_valid(&entry.key) || entry.git_root_commit.trim().is_empty() {
                return Err(std::io::Error::other(
                    "legacy repository identity memo is invalid",
                ));
            }
            if found.is_some_and(|previous| previous.git_root_commit != entry.git_root_commit) {
                return Err(std::io::Error::other(
                    "canonical root aliases have conflicting repository identities",
                ));
            }
            found = Some(entry);
        }
        Ok(found.cloned())
    }
}

#[derive(Default)]
struct LegacyMemoSlotState {
    snapshot: Mutex<Option<LegacyMemoIndex>>,
    // A tiny mirror lets registry cleanup inspect age without waiting for the
    // snapshot lock, which can be held during filesystem work.
    snapshot_built_at: Mutex<Option<Instant>>,
}

type LegacyMemoSlot = Arc<LegacyMemoSlotState>;

struct LegacyMemoSlotGuard<'a> {
    snapshot: std::sync::MutexGuard<'a, Option<LegacyMemoIndex>>,
    snapshot_built_at: &'a Mutex<Option<Instant>>,
}

impl LegacyMemoSlotState {
    fn lock(&self) -> std::sync::LockResult<LegacyMemoSlotGuard<'_>> {
        match self.snapshot.lock() {
            Ok(snapshot) => Ok(LegacyMemoSlotGuard {
                snapshot,
                snapshot_built_at: &self.snapshot_built_at,
            }),
            Err(error) => Err(std::sync::PoisonError::new(LegacyMemoSlotGuard {
                snapshot: error.into_inner(),
                snapshot_built_at: &self.snapshot_built_at,
            })),
        }
    }

    fn snapshot_expired(&self, now: Instant) -> bool {
        self.snapshot_built_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|built_at| {
                now.saturating_duration_since(built_at) >= LEGACY_IDENTITY_MEMO_MAX_AGE
            })
    }
}

impl std::ops::Deref for LegacyMemoSlotGuard<'_> {
    type Target = Option<LegacyMemoIndex>;
    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl std::ops::DerefMut for LegacyMemoSlotGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.snapshot
    }
}

impl Drop for LegacyMemoSlotGuard<'_> {
    fn drop(&mut self) {
        // Publish age as the snapshot lock is released, without requiring the
        // registry to acquire the lock used for filesystem work.
        *self
            .snapshot_built_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            self.snapshot.as_ref().map(|index| index.built_at);
    }
}

/// Retain at most four storage-root indexes, ordered by last slot acquisition,
/// and remove expired snapshots on every registry touch. A representative
/// 3,000-entry, 1.09 MB memo measured about 3.3 MiB of retained index heap on
/// 64-bit macOS; pathname lengths affect that footprint. In-flight callers may
/// keep an evicted slot alive until their current operation finishes.
#[derive(Default)]
struct LegacyMemoIndexes {
    // Least recently used first; the cache is deliberately tiny.
    roots: Vec<(PathBuf, LegacyMemoSlot)>,
}

impl LegacyMemoIndexes {
    fn slot(&mut self, path: PathBuf) -> (LegacyMemoSlot, Vec<LegacyMemoSlot>) {
        let now = Instant::now();
        let mut retired = Vec::new();
        self.roots.retain(|(_, slot)| {
            if slot.snapshot_expired(now) {
                retired.push(slot.clone());
                false
            } else {
                true
            }
        });
        if let Some(position) = self.roots.iter().position(|(root, _)| root == &path) {
            let entry = self.roots.remove(position);
            let slot = entry.1.clone();
            self.roots.push(entry);
            return (slot, retired);
        }
        let slot = LegacyMemoSlot::default();
        self.roots.push((path, slot.clone()));
        if self.roots.len() > LEGACY_IDENTITY_MEMO_MAX_STORAGE_ROOTS {
            retired.push(self.roots.remove(0).1);
        }
        (slot, retired)
    }
}

#[cfg(test)]
thread_local! {
    static LEGACY_MEMO_INDEXES_OVERRIDE: std::cell::RefCell<Option<Arc<Mutex<LegacyMemoIndexes>>>> = const { std::cell::RefCell::new(None) };
}

fn legacy_identity_memo_indexes() -> Arc<Mutex<LegacyMemoIndexes>> {
    #[cfg(test)]
    if let Some(indexes) = LEGACY_MEMO_INDEXES_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return indexes;
    }
    static INDEXES: OnceLock<Arc<Mutex<LegacyMemoIndexes>>> = OnceLock::new();
    INDEXES.get_or_init(Default::default).clone()
}

fn legacy_identity_memo_slot(storage: &Path) -> LegacyMemoSlot {
    let indexes = legacy_identity_memo_indexes();
    let (slot, retired) = {
        indexes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .slot(artifact_cache_key_memo_path(storage))
    };
    // Release the registry before freeing an index's strings and path tables.
    drop(retired);
    slot
}

/// Cache canonical spellings by the memo file's identity, length and times;
/// apply AFT writes incrementally and rebuild after ten minutes even if the file
/// is unchanged.
/// Revalidate matched spellings on every lookup so a moved alias cannot return
/// its former root's identity. Newly matching spellings, including new conflicts,
/// can be missed until refresh; the age limit bounds that discovery delay.
fn legacy_identity_memo(
    root: &Path,
    raw_key: &str,
    storage: &Path,
) -> std::io::Result<Option<ArtifactCacheKeyMemoEntry>> {
    let path = artifact_cache_key_memo_path(storage);
    let slot = legacy_identity_memo_slot(storage);
    // Only this storage's first build is serialized. Do not take the ordinary
    // process memo mutex while resolving every legacy spelling's filesystem path.
    let mut cached = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stamp = LegacyMemoStamp::at(&path);
    if let Some(index) = cached.as_ref().filter(|index| {
        stamp.is_some()
            && index.stamp == stamp
            && index.built_at.elapsed() < LEGACY_IDENTITY_MEMO_MAX_AGE
    }) {
        return index.lookup(root, raw_key);
    }
    *cached = None;
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let before = LegacyMemoStamp::of_file(&file);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let entries = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let index = LegacyMemoIndex::build(entries, before);
    let result = index.lookup(root, raw_key);
    // Read and canonicalization can race a writer. Return the opened snapshot,
    // as a full scan would, but never cache it under a replacement's stamp.
    if before.is_some()
        && before == LegacyMemoStamp::of_file(&file)
        && before == LegacyMemoStamp::at(&path)
    {
        *cached = Some(index);
    }
    result
}

#[cfg(test)]
fn legacy_identity_memo_full_scan(
    root: &Path,
    raw_key: &str,
    storage: &Path,
) -> std::io::Result<Option<ArtifactCacheKeyMemoEntry>> {
    let entries: BTreeMap<String, ArtifactCacheKeyMemoEntry> =
        match fs::read(artifact_cache_key_memo_path(storage)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    let canonical = crate::root_identity::canonical_root(root);
    let mut found: Option<ArtifactCacheKeyMemoEntry> = None;
    for (spelling, entry) in entries {
        if spelling != raw_key
            && crate::root_identity::canonical_root(Path::new(&spelling)) != canonical
        {
            continue;
        }
        if !artifact_key_looks_valid(&entry.key) || entry.git_root_commit.trim().is_empty() {
            return Err(std::io::Error::other(
                "legacy repository identity memo is invalid",
            ));
        }
        if found
            .as_ref()
            .is_some_and(|previous| previous.git_root_commit != entry.git_root_commit)
        {
            return Err(std::io::Error::other(
                "canonical root aliases have conflicting repository identities",
            ));
        }
        found = Some(entry);
    }
    Ok(found)
}

/// Session configure and StandingRoots::reconcile use the same stored path-or-Git
/// identity. With `persist=false`, health reports inspect it without creating a record.
pub(crate) fn resolve_root_identity(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
    persist: bool,
) -> Result<crate::root_identity::Identity, ArtifactCacheKeyProbeError> {
    resolve_root_identity_with_recheck(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        persist,
        true,
    )
}

/// `force` distinguishes a real bind from background standing reconciliation.
/// A provisional identity is an error here: only a session bind may use one,
/// and only through [`resolve_root_identity_for_bind`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_root_identity_with_recheck(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
    persist: bool,
    force: bool,
) -> Result<crate::root_identity::Identity, ArtifactCacheKeyProbeError> {
    let bound = resolve_root_identity_for_bind(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        persist,
        force,
    )?;
    if bound.provisional {
        return Err(ArtifactCacheKeyProbeError {
            root: memo_root.to_path_buf(),
            detail: PROVISIONAL_IDENTITY_REASON.to_string(),
            failure: ArtifactCacheKeyFailure::IdentityUnavailable,
        });
    }
    Ok(bound.identity)
}

/// Why a root has no settled artifact identity yet. Shown as the root's
/// degraded reason and as the detail of a refused identity lookup. A bound
/// root retries the identity in the background while this reason stands; see
/// [`crate::provisional_identity_retry`].
pub(crate) const PROVISIONAL_IDENTITY_REASON: &str =
    "identity provisional: aft.db was busy at bind; indexes start once the database is free";
/// Replaces [`PROVISIONAL_IDENTITY_REASON`] once the background retry gives up.
pub(crate) const PROVISIONAL_IDENTITY_EXHAUSTED_REASON: &str = "identity provisional: aft.db stayed busy or unreadable for 5 minutes after bind, so this folder has no search, semantic or call-graph index. Close whatever holds aft.db open (usually another AFT process in a long write), then reopen this folder's session to bind it again";
pub(crate) const GIT_IDENTITY_UNAVAILABLE_REASON: &str =
    "identity provisional: git_identity_unavailable";

/// Like [`resolve_root_identity_with_recheck`], but a busy aft.db yields a
/// provisional identity instead of an error. The caller must not store any
/// artifact, ownership or retention record under a provisional identity.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_root_identity_for_bind(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
    persist: bool,
    force: bool,
) -> Result<crate::root_identity::BoundIdentity, ArtifactCacheKeyProbeError> {
    // A `storage.cache_dir` cache root keeps no identity records; resolve and
    // save them on the durable root that owns it, where configure memoizes
    // the key. Any other root, and every root without `cache_dir`, is used
    // as given.
    let storage_root: &Path = &crate::root_cache::identity_storage_root(storage_root);
    let mut git_unavailable = false;
    let mut choose = |pin: crate::root_identity::LegacyPin,
                      saved: Option<&crate::root_identity::Identity>|
     -> std::io::Result<crate::root_identity::IdentityChoice> {
        use crate::root_identity::{Identity, IdentityChoice, LegacyPin};
        let pinned_path = pin == LegacyPin::Path;
        // While an older pin cannot be read, the identity chosen here is only
        // temporary, so neither refresh nor write the legacy memo from it.
        let persist = persist && pin != LegacyPin::Unreadable;
        let memo_root_key = artifact_cache_key_memo_root_key(memo_root);
        let canonical_key =
            artifact_cache_key_memo_root_key(&crate::root_identity::canonical_root(probe_root));
        // Cached lookups refresh old entries on disk. Health uses the direct,
        // non-refreshing reader instead of this mutating cache lookup.
        let cached = (persist && crate::developer_tools::git_usable())
            .then(|| {
                lookup_artifact_cache_key_memo_profiled(storage_root, &canonical_key, timings)
                    .or_else(|| {
                        lookup_artifact_cache_key_memo_profiled(
                            storage_root,
                            &memo_root_key,
                            timings,
                        )
                    })
            })
            .flatten();
        let memo = if cached.is_some() && !pinned_path {
            cached
        } else {
            match time_cache_key_work(&mut timings.legacy_memo, || {
                legacy_identity_memo(probe_root, &memo_root_key, storage_root)
            }) {
                Ok(memo) => memo,
                Err(error) if crate::developer_tools::git_usable() && !pinned_path => {
                    crate::slog_warn!(
                        "legacy identity memo unavailable; verify the repository with Git: {error}"
                    );
                    None
                }
                Err(error) => return Err(error),
            }
        };
        if pinned_path {
            if memo.is_some() {
                return Err(std::io::Error::other("a standing path identity conflicts with an existing repository memo; explicit identity migration is required"));
            }
            return Ok(IdentityChoice::retained(Identity::Path));
        }
        if !crate::developer_tools::git_usable() {
            return match memo {
                Some(entry) => {
                    let identity = Identity::git(probe_root, entry.git_root_commit)?;
                    if crate::root_identity::nearest_git_marker(probe_root)?.is_none()
                        && identity.key(probe_root, false) != entry.key
                    {
                        return Err(std::io::Error::other(
                            "legacy repository scope cannot be recovered without Git",
                        ));
                    }
                    Ok(IdentityChoice::retained(identity))
                }
                None => Ok(IdentityChoice::retained(Identity::Path)),
            };
        }
        let marker = match crate::root_identity::nearest_git_marker(probe_root) {
            Ok(Some(_)) => GitMarkerState::Present,
            Ok(None) => root_git_marker_state(probe_root, git_common_dir),
            Err(_) if saved.is_some() => {
                return Ok(IdentityChoice::retained(saved.unwrap().clone()))
            }
            Err(error) => return Err(error),
        };
        if marker == GitMarkerState::Absent && memo.is_none() && saved.is_none() {
            return Ok(IdentityChoice::retained(Identity::Path));
        }
        match repo_root_commit_with_retry_profiled(probe_root, timings) {
            RootCommitResolution::Commit(root_commit) => {
                let key = session_artifact_key_from_git_identity(probe_root, &root_commit);
                if persist {
                    if let Err(error) = record_artifact_cache_key_memo_profiled(
                        storage_root,
                        &memo_root_key,
                        &key,
                        &root_commit,
                        timings,
                    ) {
                        crate::slog_warn!(
                            "artifact cache key: could not update legacy Git memo: {error}"
                        );
                    }
                }
                Identity::git(probe_root, root_commit).map(IdentityChoice::verified)
            }
            // A verified non-repository or unborn replacement must not inherit
            // the committed repository identity retained in an older memo.
            RootCommitResolution::NotARepo => Ok(IdentityChoice::verified(Identity::Path)),
            RootCommitResolution::Failed(_) if saved.is_some() => {
                Ok(IdentityChoice::retained(saved.unwrap().clone()))
            }
            RootCommitResolution::Failed(_) if memo.is_some() => {
                Identity::git(probe_root, memo.unwrap().git_root_commit)
                    .map(IdentityChoice::retained)
            }
            RootCommitResolution::Failed(detail) => {
                git_unavailable = true;
                Err(std::io::Error::other(detail))
            }
            RootCommitResolution::Cancelled => {
                Err(std::io::Error::other("artifact cache key probe cancelled"))
            }
        }
    };
    let result = if persist {
        crate::root_identity::bind_with_recheck(probe_root, storage_root, force, choose)
    } else {
        match crate::root_identity::read(probe_root, storage_root) {
            Ok(Some(identity)) => Ok(crate::root_identity::BoundIdentity {
                identity,
                provisional: false,
            }),
            Ok(None) => {
                crate::root_identity::select(probe_root, storage_root, |pin| choose(pin, None))
            }
            Err(error) => Err(error),
        }
    };
    result.map_err(|error| ArtifactCacheKeyProbeError {
        root: memo_root.to_path_buf(),
        failure: if git_unavailable {
            ArtifactCacheKeyFailure::GitUnavailable
        } else {
            ArtifactCacheKeyFailure::IdentityUnavailable
        },
        detail: if git_unavailable {
            error.to_string()
        } else {
            format!("{error}; an existing identity was not replaced. Restore unreadable identity metadata or perform an explicit identity migration. {}", if crate::developer_tools::git_usable() { "" } else { crate::developer_tools::MISSING_DEVELOPER_TOOLS_REASON })
        },
    })
}

/// In-process root→key map recorded at every successful cache-key
/// derivation, for paths that must NEVER block. Unlike the persisted memo
/// (git identities only, lazily loaded from disk), this covers path-identity
/// roots too and never touches disk or spawns.
static DERIVED_CACHE_KEYS: OnceLock<RwLock<HashMap<PathBuf, String>>> = OnceLock::new();

fn record_derived_cache_key(project_root: &Path, key: &str) {
    let map = DERIVED_CACHE_KEYS.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(mut map) = map.write() {
        map.insert(project_root.to_path_buf(), key.to_string());
    }
}

/// Non-blocking cache-key lookup for the channel-0 health reply path:
/// consults only the in-process derivation map — no git subprocess, no disk
/// read, try-lock only. Returns `None` for a root that has not derived a key
/// this process (callers treat that as "annotation unavailable").
///
/// Why this exists: `artifact_cache_key()` spawns a git probe (up to 3 execs
/// + backoff sleeps), and on a host whose exec path is stalling — the exact
/// condition health probes exist to ride out — a per-root spawn loop pushes
/// the health reply past the supervisor's deadline and reads as module death
/// (2026-08-08 second outage). The health-path rule is not just
/// try-lock-only: NOTHING on the reply path may block, and exec is worse
/// than any lock.
pub fn artifact_cache_key_memoized_only(project_root: &Path) -> Option<String> {
    let map = DERIVED_CACHE_KEYS.get()?;
    let map = map.try_read().ok()?;
    map.get(project_root).cloned()
}

/// Keep Git-top-level artifact derivation shared with standing roots byte-for-byte.
pub(crate) fn artifact_key_from_git_identity(root_commit: &str) -> String {
    artifact_hash16(root_commit.as_bytes())
}

/// Artifact key for a session rooted at `project_root` inside the Git
/// repository whose canonical root-commit identity is `root_commit`.
///
/// A session at a checkout's top level uses the whole-repository key, which
/// the home checkout publishes and its linked worktrees borrow. A session
/// opened in a subfolder indexes only that subfolder, so it must not publish
/// under the whole-repository key: its snapshot would replace the full one and
/// every borrower would silently lose the files outside the subfolder. It
/// gets a key of its own instead, derived like a standing root below a
/// checkout (`scoped_key::scoped_v1_key` over the repository identity and the
/// forward-slash path below the top level). The same subfolder of the home
/// checkout and of a linked worktree therefore still share one artifact.
///
/// The key is cut to the 16 hex characters every other session key has,
/// because session storage (orphan sweeps, the key memo, transient caches)
/// only recognises that shape. The full-length scoped key stays reserved for
/// standing roots, so a session never co-writes a standing root's artifact.
pub(crate) fn session_artifact_key_from_git_identity(
    project_root: &Path,
    root_commit: &str,
) -> String {
    match checkout_position(project_root) {
        CheckoutPosition::TopLevel => artifact_key_from_git_identity(root_commit),
        CheckoutPosition::Subdirectory(relative) => {
            match crate::scoped_key::scoped_v1_key(root_commit, relative.as_bytes()) {
                Ok(scoped) => scoped[..16].to_string(),
                Err(_) => artifact_path_identity_key(project_root),
            }
        }
        // A subfolder whose path cannot be spelled as a logical relative path
        // still must not share the repository key; its own path is unique.
        CheckoutPosition::Unrepresentable => artifact_path_identity_key(project_root),
    }
}

/// Where a directory sits inside the Git checkout that contains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CheckoutPosition {
    /// The checkout's top level, or a directory with no enclosing checkout.
    TopLevel,
    /// A directory below the top level, as a forward-slash relative path.
    Subdirectory(String),
    /// A directory below the top level whose relative path is not valid
    /// Unicode or otherwise cannot be written as a logical relative path.
    Unrepresentable,
}

/// Locate `project_root` relative to the nearest enclosing checkout top level:
/// the closest directory, starting at `project_root` itself, that holds a
/// `.git` entry (a directory in a home checkout, a file in a linked worktree
/// or submodule). This reads the filesystem only; it never runs git.
///
/// Both sides of any comparison are derived through this one function from
/// the same canonicalized form, so the result is stable across the Windows
/// verbatim and plain spellings of one path.
pub(crate) fn checkout_position(project_root: &Path) -> CheckoutPosition {
    let root = canonicalize_or_normalize(project_root);
    let has_git_marker = |dir: &Path| fs::symlink_metadata(dir.join(".git")).is_ok();
    if has_git_marker(&root) {
        return CheckoutPosition::TopLevel;
    }
    for ancestor in root.ancestors().skip(1) {
        if has_git_marker(ancestor) {
            return match crate::scoped_key::scoped_relative_path(&root, ancestor) {
                Ok(relative) => CheckoutPosition::Subdirectory(relative),
                Err(_) => CheckoutPosition::Unrepresentable,
            };
        }
    }
    CheckoutPosition::TopLevel
}

/// Existing non-Git path-scope artifact derivation, exposed for the standing
/// identity classifier so it does not mint a parallel path-key family.
pub(crate) fn artifact_path_identity_key(project_root: &Path) -> String {
    let canonical_root = canonicalize_or_normalize(project_root);
    artifact_hash16(canonical_root.to_string_lossy().as_bytes())
}

#[cfg(test)]
pub(crate) fn artifact_path_identity_key_for_test(project_root: &Path) -> String {
    artifact_path_identity_key(project_root)
}

fn artifact_hash16(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = format!("{:x}", hasher.finalize());
    digest[..16].to_string()
}

fn artifact_cache_key_memo_root_key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

fn artifact_cache_key_memo_path(storage_root: &Path) -> PathBuf {
    storage_root.join(ARTIFACT_CACHE_KEY_MEMO_FILE)
}

fn artifact_cache_key_memo_state() -> &'static Mutex<ArtifactCacheKeyMemoState> {
    ARTIFACT_CACHE_KEY_MEMO_STATE.get_or_init(|| Mutex::new(ArtifactCacheKeyMemoState::default()))
}

impl ArtifactCacheKeyMemoState {
    fn entries_for_storage_root(
        &mut self,
        storage_root: &Path,
        timings: &mut ArtifactCacheKeyTimings,
    ) -> &mut BTreeMap<String, ArtifactCacheKeyMemoEntry> {
        if !self.by_storage_root.contains_key(storage_root) {
            let entries = time_cache_key_work(&mut timings.memo_load, || {
                read_artifact_cache_key_memo_file(storage_root)
            });
            self.by_storage_root
                .insert(storage_root.to_path_buf(), entries);
        }
        self.by_storage_root
            .get_mut(storage_root)
            .expect("memo storage root inserted")
    }
}

#[cfg(test)]
fn lookup_artifact_cache_key_memo(
    storage_root: &Path,
    memo_root_key: &str,
) -> Option<ArtifactCacheKeyMemoEntry> {
    lookup_artifact_cache_key_memo_profiled(
        storage_root,
        memo_root_key,
        &mut ArtifactCacheKeyTimings::default(),
    )
}

fn lookup_artifact_cache_key_memo_profiled(
    storage_root: &Path,
    memo_root_key: &str,
    timings: &mut ArtifactCacheKeyTimings,
) -> Option<ArtifactCacheKeyMemoEntry> {
    let waiting = Instant::now();
    let mut state = artifact_cache_key_memo_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    timings.memo_wait += waiting.elapsed();
    let holding = Instant::now();
    let result = (|| {
        let entries = state.entries_for_storage_root(storage_root, timings);
        let entry = entries.get(memo_root_key)?.clone();
        let now = current_time_millis();
        if Path::new(memo_root_key).exists()
            && now.saturating_sub(entry.recorded_at_ms)
                >= ARTIFACT_CACHE_KEY_MEMO_READ_REFRESH_AGE.as_millis() as u64
        {
            // Read-only roots may reuse the same borrowed artifact key for months without
            // deriving a new key. Refresh at most daily to keep the cached key available
            // while avoiding a disk write on every failed lookup.
            let mut refreshed = entry.clone();
            refreshed.recorded_at_ms = now;
            entries.insert(memo_root_key.to_string(), refreshed);
            if let Err(error) = time_cache_key_work(&mut timings.memo_write, || {
                write_artifact_cache_key_memo_file(storage_root, entries)
            }) {
                entries.insert(memo_root_key.to_string(), entry.clone());
                crate::slog_warn!(
                    "artifact cache key: failed to refresh memo for {} in {}: {}",
                    memo_root_key,
                    storage_root.display(),
                    error
                );
            }
        }
        entries.get(memo_root_key).cloned()
    })();
    timings.memo_hold += holding.elapsed();
    result
}

#[cfg(test)]
fn record_artifact_cache_key_memo(
    storage_root: &Path,
    memo_root_key: &str,
    key: &str,
    git_root_commit: &str,
) -> std::io::Result<()> {
    record_artifact_cache_key_memo_profiled(
        storage_root,
        memo_root_key,
        key,
        git_root_commit,
        &mut ArtifactCacheKeyTimings::default(),
    )
}

fn record_artifact_cache_key_memo_profiled(
    storage_root: &Path,
    memo_root_key: &str,
    key: &str,
    git_root_commit: &str,
    timings: &mut ArtifactCacheKeyTimings,
) -> std::io::Result<()> {
    let waiting = Instant::now();
    let mut state = artifact_cache_key_memo_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    timings.memo_wait += waiting.elapsed();
    let holding = Instant::now();
    let result = (|| {
        let entries = state.entries_for_storage_root(storage_root, timings);
        if entries
            .get(memo_root_key)
            .is_some_and(|entry| entry.key == key && entry.git_root_commit == git_root_commit)
        {
            return Ok(());
        }
        let now = current_time_millis();
        entries.insert(
            memo_root_key.to_string(),
            ArtifactCacheKeyMemoEntry {
                key: key.to_string(),
                git_root_commit: git_root_commit.to_string(),
                recorded_at_ms: now,
            },
        );
        time_cache_key_work(&mut timings.memo_prune, || {
            prune_expired_artifact_cache_key_memo_entries(entries, now)
        });
        time_cache_key_work(&mut timings.memo_write, || {
            write_artifact_cache_key_memo_file(storage_root, entries)
        })
    })();
    timings.memo_hold += holding.elapsed();
    result
}

fn prune_expired_artifact_cache_key_memo_entries(
    entries: &mut BTreeMap<String, ArtifactCacheKeyMemoEntry>,
    now: u64,
) {
    entries.retain(|root, entry| {
        Path::new(root).exists()
            || now.saturating_sub(entry.recorded_at_ms)
                <= ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64
    });
}

fn read_artifact_cache_key_memo_file(
    storage_root: &Path,
) -> BTreeMap<String, ArtifactCacheKeyMemoEntry> {
    let path = artifact_cache_key_memo_path(storage_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return BTreeMap::new(),
    };
    let entries =
        match serde_json::from_slice::<BTreeMap<String, ArtifactCacheKeyMemoEntry>>(&bytes) {
            Ok(entries) => entries,
            Err(error) => {
                crate::slog_warn!(
                    "artifact cache key: ignoring corrupt memo file {}: {}",
                    path.display(),
                    error
                );
                return BTreeMap::new();
            }
        };
    entries
        .into_iter()
        .filter(|(root, entry)| {
            !root.is_empty()
                && artifact_key_looks_valid(&entry.key)
                && !entry.git_root_commit.trim().is_empty()
        })
        .collect()
}

fn write_artifact_cache_key_memo_file(
    storage_root: &Path,
    entries: &BTreeMap<String, ArtifactCacheKeyMemoEntry>,
) -> std::io::Result<()> {
    crate::private_storage::open_root(storage_root)?;
    let path = artifact_cache_key_memo_path(storage_root);
    let temp_path = storage_root.join(format!(
        ".{ARTIFACT_CACHE_KEY_MEMO_FILE}.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));
    let bytes = serde_json::to_vec_pretty(entries).map_err(std::io::Error::other)?;
    let mut file = crate::private_storage::create(&temp_path)?;
    file.write_all(&bytes)?;
    // Close write handles before sampling the published times: Windows may
    // defer its last-write timestamp until then. A read handle keeps the same
    // file's identity available through rename without delaying that update.
    let identity = crate::file_identity::FileIdentity::of_file(&file);
    let published_file = File::open(&temp_path).ok().filter(|opened| {
        identity != crate::file_identity::FileIdentity::Unavailable
            && crate::file_identity::FileIdentity::of_file(opened) == identity
    });
    drop(file);
    let slot = legacy_identity_memo_slot(storage_root);
    let mut cached = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous_stamp = LegacyMemoStamp::at(&path);
    let can_advance = cached.as_ref().is_some_and(|index| {
        previous_stamp.is_some()
            && index.stamp == previous_stamp
            && index.built_at.elapsed() < LEGACY_IDENTITY_MEMO_MAX_AGE
    });
    if let Err(error) = fs::rename(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    #[cfg(test)]
    if let Some(hook) = AFTER_LEGACY_MEMO_RENAME.with(|slot| slot.borrow_mut().take()) {
        hook(&path);
    }
    // Keep the written file open across rename. A stamp sampled only from the
    // destination could belong to another writer's replacement, not these bytes.
    let published_stamp = published_file.as_ref().and_then(LegacyMemoStamp::of_file);
    if can_advance && published_stamp.is_some() && published_stamp == LegacyMemoStamp::at(&path) {
        cached
            .as_mut()
            .unwrap()
            .apply_write(entries, published_stamp.unwrap());
        if published_stamp != published_file.as_ref().and_then(LegacyMemoStamp::of_file)
            || published_stamp != LegacyMemoStamp::at(&path)
        {
            *cached = None;
        }
    } else {
        *cached = None;
    }
    Ok(())
}

fn artifact_key_looks_valid(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, PartialEq, Eq)]
enum GitMarkerState {
    Present,
    Absent,
    Unknown(String),
}

fn root_git_marker_state(project_root: &Path, git_common_dir: Option<&Path>) -> GitMarkerState {
    if git_common_dir.is_some() {
        return GitMarkerState::Present;
    }
    let git_marker = project_root.join(".git");
    match fs::symlink_metadata(&git_marker) {
        Ok(_) => GitMarkerState::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GitMarkerState::Absent,
        Err(error) => GitMarkerState::Unknown(format!(
            "failed to inspect git marker {}: {}",
            git_marker.display(),
            error
        )),
    }
}

/// Return the canonical root-commit identity used by Git artifact keys.
///
/// Scoped roots consume this exact result as an opaque byte sequence; they must
/// not independently rebuild the sorted, deduplicated root set.
pub(crate) fn canonical_git_root_commit_identity(
    project_root: &Path,
) -> Result<String, ArtifactCacheKeyProbeError> {
    match repo_root_commit_with_retry(project_root) {
        RootCommitResolution::Commit(identity) => Ok(identity),
        RootCommitResolution::NotARepo => Err(ArtifactCacheKeyProbeError {
            root: project_root.to_path_buf(),
            detail: "not a git repository".to_string(),
            failure: ArtifactCacheKeyFailure::IdentityUnavailable,
        }),
        RootCommitResolution::Failed(detail) => Err(ArtifactCacheKeyProbeError {
            root: project_root.to_path_buf(),
            detail,
            failure: ArtifactCacheKeyFailure::GitUnavailable,
        }),
        RootCommitResolution::Cancelled => Err(ArtifactCacheKeyProbeError {
            root: project_root.to_path_buf(),
            detail: "artifact cache key probe cancelled".to_string(),
            failure: ArtifactCacheKeyFailure::IdentityUnavailable,
        }),
    }
}

/// The repository identity of `project_root` for the data layer's family
/// key: the canonical root-commit identity, `None` when Git confirms the
/// folder is not a repository (or has no commit yet), and an error when Git
/// could not tell, so the caller can wait rather than key the folder by its
/// path and later find it belongs to a repository.
pub(crate) fn root_commit_identity_or_folder(
    project_root: &Path,
) -> Result<Option<String>, String> {
    match repo_root_commit_with_retry(project_root) {
        RootCommitResolution::Commit(identity) => Ok(Some(identity)),
        RootCommitResolution::NotARepo => Ok(None),
        RootCommitResolution::Failed(detail) => Err(detail),
        RootCommitResolution::Cancelled => Err("the identity probe was cancelled".to_string()),
    }
}

/// Resolve the repository root commit, retrying transient git failures.
///
/// The distinction matters because the fallback is not benign: two clones of
/// one repo that key differently (one by commit, one by path) each claim
/// artifact ownership and write the shared cache concurrently. A git
/// invocation that fails under load (spawn failure, resource exhaustion) must
/// therefore be retried, and callers that need stable identity can refuse path
/// fallback when the result is still ambiguous after retry.
fn repo_root_commit_with_retry(project_root: &Path) -> RootCommitResolution {
    repo_root_commit_with_probe(project_root, || {
        repo_root_commit_with_retry_profiled(project_root, &mut ArtifactCacheKeyTimings::default())
    })
}

fn repo_root_commit_with_probe(
    project_root: &Path,
    probe: impl FnOnce() -> RootCommitResolution,
) -> RootCommitResolution {
    // Callers on this path (the call-graph store refreshes after every edit,
    // inspect scans, scoped keys) re-derive the key for the same root many
    // times a minute. Reuse the answer while the repository files that decide
    // HEAD and its history view are unchanged.
    let stamp = RootCommitStamp::capture(project_root);
    if let Some(stamp) = stamp.as_ref() {
        if let Some(cached) = cached_root_commit(project_root, stamp) {
            return RootCommitResolution::Commit(cached);
        }
    }
    let resolution = probe();
    // A checkout or history-view change during the probe can pair the result
    // with an unrelated stamp. Recheck before caching, but return the probe's
    // result even when the repository moved. A repository without a commit
    // must also be probed again once its first commit lands.
    if let (Some(stamp), RootCommitResolution::Commit(commit)) = (stamp, &resolution) {
        if RootCommitStamp::capture(project_root).as_ref() == Some(&stamp) {
            root_commit_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(project_root.to_path_buf(), (stamp, commit.clone()));
        }
    }
    resolution
}

/// Root-commit identities by project root, each with the repository state it
/// was read under.
fn root_commit_cache() -> &'static Mutex<HashMap<PathBuf, (RootCommitStamp, String)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (RootCommitStamp, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_root_commit(project_root: &Path, stamp: &RootCommitStamp) -> Option<String> {
    #[cfg(test)]
    if GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
    {
        // Tests that script the probe expect every lookup to reach it.
        return None;
    }
    root_commit_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(project_root)
        .filter(|(cached, _)| cached == stamp)
        .map(|(_, commit)| commit.clone())
}

/// Cache only the digest, not Git configuration contents. Background identity
/// checks watch directory/HEAD inode and time metadata as well as history inputs.
/// An unmodeled history view has no reusable stamp, even if those paths are stable.
pub(crate) fn repository_identity_stamp(root: &Path) -> Option<String> {
    let history = RootCommitStamp::capture(root)?;
    let marker = crate::root_identity::nearest_git_marker(root)
        .map(|directory| directory.map(|directory| directory.join(".git")));
    let mut paths = Vec::new();
    if let Ok(Some(marker)) = &marker {
        paths.push(marker.clone());
        let git_dir = if fs::metadata(marker).is_ok_and(|metadata| metadata.is_dir()) {
            marker.clone()
        } else {
            fs::read_to_string(marker)
                .ok()
                .and_then(|text| {
                    text.trim_end_matches(['\r', '\n'])
                        .strip_prefix("gitdir:")
                        .map(|pointer| marker.parent().unwrap().join(pointer.trim_start()))
                })
                .unwrap_or_else(|| marker.clone())
        };
        let git_dir = crate::root_identity::canonical_root(&git_dir);
        if !paths.contains(&git_dir) {
            paths.push(git_dir.clone());
        }
        paths.push(crate::root_identity::canonical_root(&git_dir.join("HEAD")));
        paths.push(git_dir.join("commondir"));
        if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
            paths.push(crate::root_identity::canonical_root(
                &git_dir.join(common.trim_end_matches(['\r', '\n'])),
            ));
        }
    }
    let watched: Vec<_> = paths
        .iter()
        .map(|path| (path, RootCommitFileIdentity::of(path, true)))
        .collect();
    let git_env: BTreeMap<_, _> = std::env::vars_os()
        .filter(|(name, _)| name.to_str().is_some_and(|name| name.starts_with("GIT_")))
        .collect();
    Some(
        blake3::hash(format!("{marker:?}|{watched:?}|{history:?}|{git_env:?}").as_bytes())
            .to_hex()
            .to_string(),
    )
}

/// The repository state that decides `git rev-list --max-parents=0 HEAD`.
///
/// History is immutable, so the root set changes only when HEAD names another
/// commit or the history view changes. HEAD, its loose branch ref, the local
/// config, and loose replace-ref contents are kept by content; packed refs,
/// shallow boundaries, grafts and the replace-ref directory use file identity.
/// Config includes and unmodeled Git environment overrides make `capture`
/// decline caching, so Git is asked on every lookup instead of serving a stale
/// root.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RootCommitStamp {
    common_dir: PathBuf,
    common_dir_identity: RootCommitFileIdentity,
    git_marker: Option<RootCommitFileIdentity>,
    git_marker_text: Option<Vec<u8>>,
    commondir_text: Option<Vec<u8>>,
    head: Vec<u8>,
    head_ref: Option<Vec<u8>>,
    config_contents: Vec<Option<Vec<u8>>>,
    replace_ref_contents: Vec<(PathBuf, Vec<u8>)>,
    watched: Vec<Option<RootCommitFileIdentity>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootCommitFileIdentity {
    #[cfg(unix)]
    inode: (u64, u64),
    /// Volume serial number and file index: a file renamed over another is a
    /// different file, even when its size and times match because both were
    /// written within one tick of the Windows file clock (about 15.6 ms).
    /// Unix keeps the device and inode in `inode` above instead, so the
    /// digest `repository_identity_stamp` hands to root identity records
    /// (built from this type's `Debug` text) is unchanged there.
    #[cfg(not(unix))]
    identity: crate::file_identity::FileIdentity,
    /// Size, modification and status-change times. Left out for a directory
    /// whose entries change on ordinary git use (the `.git` directory gains
    /// and renames `index.lock` on every status refresh); only its
    /// replacement matters there.
    contents: Option<(u64, Option<SystemTime>, i64, i64)>,
}

impl RootCommitFileIdentity {
    fn of(path: &Path, directory_contents: bool) -> Option<Option<Self>> {
        match crate::file_identity::symlink_metadata(path) {
            Ok((metadata, identity)) => {
                // Link identity does not cover changes to the target's history
                // data, so leave linked files and directories uncached.
                if metadata.file_type().is_symlink() {
                    return None;
                }
                #[cfg(unix)]
                use std::os::unix::fs::MetadataExt;
                #[cfg(unix)]
                let changed = (metadata.ctime(), metadata.ctime_nsec());
                // Unix reads the same device and inode from `metadata` below.
                #[cfg(unix)]
                let _ = identity;
                #[cfg(not(unix))]
                let changed = (0, 0);
                Some(Some(Self {
                    #[cfg(unix)]
                    inode: (metadata.dev(), metadata.ino()),
                    #[cfg(not(unix))]
                    identity,
                    contents: (!metadata.is_dir() || directory_contents).then(|| {
                        (
                            metadata.len(),
                            metadata.modified().ok(),
                            changed.0,
                            changed.1,
                        )
                    }),
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
        }
    }
}

impl RootCommitStamp {
    /// Share immutable history only within one object store and one HEAD.
    /// Special history views still use the full Git probe on every bind.
    fn shared_history_key(&self) -> Option<(String, String)> {
        if !self.replace_ref_contents.is_empty()
            || std::env::var_os("GIT_NAMESPACE").is_some()
            || std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES").is_some()
            || std::env::var_os("GIT_REFERENCE_BACKEND").is_some()
        {
            return None;
        }
        for contents in self.config_contents.iter().flatten() {
            for token in [b"partialclone".as_slice(), b"promisor", b"refstorage"] {
                if contents
                    .windows(token.len())
                    .any(|window| window.eq_ignore_ascii_case(token))
                {
                    return None;
                }
            }
        }
        for path in [
            self.common_dir.join("shallow"),
            self.common_dir.join("info/grafts"),
            self.common_dir.join("objects/info/alternates"),
        ] {
            if !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                return None;
            }
        }
        let packed = match fs::read(self.common_dir.join("packed-refs")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => return None,
        };
        if packed
            .windows(b"refs/replace/".len())
            .any(|window| window == b"refs/replace/")
        {
            return None;
        }
        let head = std::str::from_utf8(&self.head).ok()?.trim();
        let commit = if let Some(reference) = head.strip_prefix("ref:") {
            match self.head_ref.as_deref() {
                Some(bytes) => std::str::from_utf8(bytes).ok()?.trim(),
                None => std::str::from_utf8(&packed)
                    .ok()?
                    .lines()
                    .find_map(|line| {
                        let (oid, name) = line.split_once(' ')?;
                        (name == reference.trim()).then_some(oid)
                    })?,
            }
        } else {
            head
        };
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        // A commit's ancestry is content-addressed. New loose objects, repacks
        // and commit-graph updates cannot change which parentless commits are
        // reachable from the same HEAD, so storage topology must not invalidate
        // sharing across busy worktrees.
        // History-changing views are excluded above; each bind still asks Git
        // to verify repository access and a readable HEAD before reusing roots.
        let git_env: BTreeMap<_, _> = std::env::vars_os()
            .filter(|(name, _)| name.to_str().is_some_and(|name| name.starts_with("GIT_")))
            .collect();
        let key = blake3::hash(
            format!(
                "{:?}|{commit}|{:?}|{:?}|{:?}|{git_env:?}",
                self.common_dir, self.common_dir_identity, self.config_contents, self.watched
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        Some((key, commit.to_string()))
    }

    /// Environment variables that move git's repository, work tree or
    /// history view away from the on-disk layout this stamp reads.
    const UNMODELED_ENV: &'static [&'static str] = &[
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_OBJECT_DIRECTORY",
        "GIT_REPLACE_REF_BASE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
    ];

    fn capture(project_root: &Path) -> Option<Self> {
        if Self::UNMODELED_ENV
            .iter()
            .any(|key| std::env::var_os(key).is_some())
        {
            return None;
        }
        // Git discovers the repository from the nearest ancestor holding a
        // `.git` entry: a directory in a home checkout, a `gitdir:` file in a
        // linked worktree or submodule.
        let root = canonicalize_or_normalize(project_root);
        let marker = root
            .ancestors()
            .map(|directory| directory.join(".git"))
            .find(|marker| fs::symlink_metadata(marker).is_ok())?;
        let marker_metadata = fs::symlink_metadata(&marker).ok()?;
        let (git_dir, git_marker_text) = if marker_metadata.is_dir() {
            (marker.clone(), None)
        } else if marker_metadata.is_file() {
            let text = fs::read(&marker).ok()?;
            let pointer = std::str::from_utf8(&text)
                .ok()?
                .trim()
                .strip_prefix("gitdir:")?
                .trim();
            (marker.parent()?.join(pointer), Some(text))
        } else {
            return None;
        };
        let commondir_file = git_dir.join("commondir");
        let (common_dir, commondir_text) = match fs::read(&commondir_file) {
            Ok(text) => {
                let pointer = std::str::from_utf8(&text).ok()?.trim();
                (git_dir.join(pointer), Some(text))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (git_dir.clone(), None),
            Err(_) => return None,
        };
        let head = fs::read(git_dir.join("HEAD")).ok()?;
        // The reftable ref backend keeps refs outside the files read here.
        if fs::symlink_metadata(common_dir.join("reftable")).is_ok() {
            return None;
        }
        let head_ref = match std::str::from_utf8(&head).ok()?.trim().strip_prefix("ref:") {
            Some(name) => {
                let name = name.trim();
                // Only a branch, which lives under the common directory;
                // per-worktree and other refs are left to git.
                if !name.starts_with("refs/heads/")
                    || name.split('/').any(|part| part.is_empty() || part == "..")
                {
                    return None;
                }
                match fs::read(common_dir.join(name)) {
                    // A symbolic branch can change its target without its
                    // own bytes changing, so the stamp could not see it.
                    Ok(target) if target.starts_with(b"ref:") => return None,
                    Ok(target) => Some(target),
                    // A packed ref: the packed-refs identity below covers it.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => return None,
                }
            }
            None => None,
        };
        let mut config_paths = vec![common_dir.join("config")];
        let worktree_config = git_dir.join("config.worktree");
        if !config_paths.contains(&worktree_config) {
            config_paths.push(worktree_config);
        }
        let mut config_contents = Vec::with_capacity(config_paths.len());
        let mut watched = Vec::with_capacity(config_paths.len() + 4);
        for path in config_paths {
            let identity = RootCommitFileIdentity::of(&path, false)?;
            let contents = match fs::read(&path) {
                Ok(contents) => Some(contents),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return None,
            };
            // Git can include configuration from outside the repository. Its
            // contents would affect replace-ref handling without being covered
            // by this stamp, so conservatively disable caching for includes.
            if contents
                .as_deref()
                .is_some_and(config_may_include_external_files)
            {
                return None;
            }
            config_contents.push(contents);
            watched.push(identity);
        }
        let replace_dir = common_dir.join("refs").join("replace");
        let replace_ref_contents = read_loose_replace_refs(&replace_dir)?;
        for path in [
            common_dir.join("packed-refs"),
            common_dir.join("shallow"),
            common_dir.join("info").join("grafts"),
            replace_dir,
        ] {
            // The directory catches additions and removals; each loose ref's
            // bytes above catch retargeting an existing replacement.
            watched.push(RootCommitFileIdentity::of(&path, true)?);
        }
        Some(Self {
            common_dir: fs::canonicalize(&common_dir).ok()?,
            common_dir_identity: RootCommitFileIdentity::of(&common_dir, false)??,
            git_marker: RootCommitFileIdentity::of(&marker, false)?,
            git_marker_text,
            commondir_text,
            head,
            head_ref,
            config_contents,
            replace_ref_contents,
            watched,
        })
    }
}

fn config_may_include_external_files(contents: &[u8]) -> bool {
    // Config section names are case-insensitive. Matching this token anywhere
    // may reject harmless text in comments or values, but cannot miss an
    // include directive whose source would not otherwise be stamped.
    contents
        .windows(b"include".len())
        .any(|window| window.eq_ignore_ascii_case(b"include"))
}

fn read_loose_replace_refs(directory: &Path) -> Option<Vec<(PathBuf, Vec<u8>)>> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        _ => return None,
    }

    let mut replace_refs = Vec::new();
    for entry in fs::read_dir(directory).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        replace_refs.push((PathBuf::from(entry.file_name()), fs::read(path).ok()?));
    }
    replace_refs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Some(replace_refs)
}

#[cfg(test)]
thread_local! {
    /// Real root-commit git runs made by this thread, by project root.
    static ROOT_COMMIT_GIT_RUNS: std::cell::RefCell<HashMap<PathBuf, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

#[cfg(test)]
pub(crate) fn root_commit_git_runs_for_test(project_root: &Path) -> usize {
    ROOT_COMMIT_GIT_RUNS.with(|runs| runs.borrow().get(project_root).copied().unwrap_or(0))
}

fn repo_root_commit_with_retry_profiled(
    project_root: &Path,
    timings: &mut ArtifactCacheKeyTimings,
) -> RootCommitResolution {
    for attempt in 0..3u32 {
        if root_commit_probe_cancelled() {
            return RootCommitResolution::Cancelled;
        }
        let probe = git_root_commit_once(project_root, timings);
        if root_commit_probe_cancelled() {
            return RootCommitResolution::Cancelled;
        }
        match probe {
            RootCommitProbe::Commit(commit) => return RootCommitResolution::Commit(commit),
            RootCommitProbe::NotARepo => return RootCommitResolution::NotARepo,
            RootCommitProbe::NoCommit => return RootCommitResolution::NotARepo,
            RootCommitProbe::Transient(detail) => {
                if attempt == 2 {
                    return RootCommitResolution::Failed(detail);
                }
                if root_commit_probe_cancelled() {
                    return RootCommitResolution::Cancelled;
                }
                time_cache_key_work(&mut timings.retry_wait, || {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt as u64 + 1)));
                });
                if root_commit_probe_cancelled() {
                    return RootCommitResolution::Cancelled;
                }
            }
        }
    }
    RootCommitResolution::Failed("git root-commit probe retry loop exhausted".to_string())
}

fn root_commit_probe_cancelled() -> bool {
    crate::executor::current_job_cancellation()
        .is_some_and(|token| token.cancel_requested_before_commit())
}

enum RootCommitResolution {
    Commit(String),
    NotARepo,
    Failed(String),
    Cancelled,
}

enum RootCommitProbe {
    Commit(String),
    /// Deterministic: not a git work tree.
    NotARepo,
    /// Deterministic but still git-like: a repository exists but has no commit identity yet.
    NoCommit,
    /// Ambiguous failure (spawn error, killed, unexpected git error): retry.
    Transient(String),
}

fn git_root_commit_once(
    project_root: &Path,
    timings: &mut ArtifactCacheKeyTimings,
) -> RootCommitProbe {
    #[cfg(test)]
    if let Some(override_probe) = GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        if let Some(result) = override_probe(project_root) {
            return result;
        }
    }

    git_root_commit_once_real(project_root, timings)
}

/// Canonicalize the root set before it becomes an artifact-cache identity.
/// Grafted-history repositories can have multiple roots, and Git traversal
/// order may change after repacks or commit-graph regeneration.
fn canonicalize_root_commit_output(stdout: &[u8]) -> RootCommitProbe {
    let decoded = String::from_utf8_lossy(stdout);
    let mut roots: Vec<&str> = decoded
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    roots.sort_unstable();
    roots.dedup();

    if roots.is_empty() {
        RootCommitProbe::NoCommit
    } else {
        RootCommitProbe::Commit(roots.join("\n"))
    }
}

fn git_root_commit_once_real(
    project_root: &Path,
    timings: &mut ArtifactCacheKeyTimings,
) -> RootCommitProbe {
    // Keep the availability timing, but preserve uncertainty: a disabled Git
    // cannot justify switching a repository's stored data to path identity.
    if !time_cache_key_work(&mut timings.git_usable, crate::developer_tools::git_usable) {
        return RootCommitProbe::Transient(
            crate::developer_tools::MISSING_DEVELOPER_TOOLS_REASON.to_string(),
        );
    }
    // A fresh worktree still asks Git to verify access and HEAD: filesystem
    // stamps alone cannot attest ownership policy or a readable commit object.
    // Only the parent walk is shared with an earlier verified bind. Keep the
    // per-root identity lock, legacy database pins that preserve a root's
    // stored identity, and scoped-key derivation outside this cache.
    type SharedHistory = HashMap<PathBuf, (String, String)>;
    static SHARED_HISTORY: OnceLock<Mutex<SharedHistory>> = OnceLock::new();
    let history = SHARED_HISTORY.get_or_init(|| Mutex::new(HashMap::new()));
    let stamp = RootCommitStamp::capture(project_root);
    let shared = stamp.as_ref().and_then(RootCommitStamp::shared_history_key);
    let cached = shared.as_ref().and_then(|(key, _)| {
        history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&stamp.as_ref().unwrap().common_dir)
            .filter(|(saved, _)| saved == key)
            .map(|(_, roots)| roots.clone())
    });
    // Tests count each Git verification of the root, whether it reuses the
    // shared history (HEAD check only) or walks it.
    #[cfg(test)]
    ROOT_COMMIT_GIT_RUNS.with(|runs| {
        *runs
            .borrow_mut()
            .entry(project_root.to_path_buf())
            .or_default() += 1;
    });
    if let Some(roots) = cached {
        let mut spawned = 0;
        let head = time_cache_key_work(&mut timings.rev_list, || {
            crate::git_probe::head_commit(project_root, &mut spawned)
        });
        timings.git_spawns += spawned;
        if head.is_ok_and(|head| {
            std::str::from_utf8(&head).is_ok_and(|head| head.trim() == shared.as_ref().unwrap().1)
        }) && RootCommitStamp::capture(project_root).as_ref() == stamp.as_ref()
            && stamp.as_ref().and_then(RootCommitStamp::shared_history_key) == shared
        {
            return RootCommitProbe::Commit(roots);
        }
    }
    // The rev_list timing also covers the extra Git checks that verify an
    // unborn branch, since they are part of deciding this probe's answer, and
    // git_spawns counts every Git process the probe actually started.
    let mut spawned = 0;
    timings.root_walks += 1;
    let result = time_cache_key_work(&mut timings.rev_list, || {
        crate::git_probe::root_commits(project_root, &mut spawned)
    });
    timings.git_spawns += spawned;
    match result {
        Ok(stdout) => {
            let result = canonicalize_root_commit_output(&stdout);
            if let (Some(stamp), Some((key, _)), RootCommitProbe::Commit(roots)) =
                (stamp.as_ref(), shared, &result)
            {
                if RootCommitStamp::capture(project_root).as_ref() == Some(stamp)
                    && stamp.shared_history_key().as_ref().map(|(key, _)| key) == Some(&key)
                {
                    history
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(stamp.common_dir.clone(), (key, roots.clone()));
                }
            }
            result
        }
        Err(crate::git_probe::ProbeError::Spawn(error)) => {
            RootCommitProbe::Transient(format!("spawn failed: {error}"))
        }
        Err(crate::git_probe::ProbeError::Failed { kind, output }) => match kind {
            crate::git_probe::FailureKind::NotRepository => RootCommitProbe::NotARepo,
            crate::git_probe::FailureKind::NoCommit => RootCommitProbe::NoCommit,
            crate::git_probe::FailureKind::Other => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                RootCommitProbe::Transient(format!(
                    "exit {:?}: {}",
                    output.status.code(),
                    stderr.trim().chars().take(200).collect::<String>()
                ))
            }
        },
    }
}

#[cfg(test)]
pub(crate) struct GitRootCommitProbeOverrideGuard {
    previous: Option<RootCommitProbeOverride>,
}

#[cfg(test)]
impl Drop for GitRootCommitProbeOverrideGuard {
    fn drop(&mut self) {
        set_git_root_commit_probe_override_for_test(self.previous.take());
    }
}

#[cfg(test)]
pub(crate) fn git_root_commit_probe_override_lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn force_git_root_commit_probe_transient_for_paths_for_test(
    roots: Vec<PathBuf>,
    detail: impl Into<String>,
) -> GitRootCommitProbeOverrideGuard {
    let detail = Arc::new(detail.into());
    install_git_root_commit_probe_override_for_test(move |project_root| {
        roots
            .iter()
            .any(|root| root == project_root)
            .then(|| RootCommitProbe::Transient((*detail).clone()))
    })
}

#[cfg(test)]
/// Force the git root-commit probe to return Transient for the given roots,
/// counting the attempts it makes and holding the first attempt in flight
/// until `release` delivers a message or its sender is dropped.
///
/// A transient result sends the probe round the retry ladder again, so a
/// cancelled configure must stop making attempts. Holding the first attempt
/// lets a test cancel while that attempt is known to be in flight, so the
/// attempt count before and after the cancel is fixed by the order of events
/// rather than by how fast the machine runs. Dropping the sender also releases
/// the hold, so a test that panics never leaves the probing thread parked.
pub(crate) fn force_git_root_commit_probe_held_transient_for_paths_for_test(
    roots: Vec<PathBuf>,
    attempts: Arc<AtomicUsize>,
    release: std::sync::mpsc::Receiver<()>,
) -> GitRootCommitProbeOverrideGuard {
    let release = Mutex::new(Some(release));
    install_git_root_commit_probe_override_for_test(move |project_root| {
        roots.iter().any(|root| root == project_root).then(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let first_attempt_hold = release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(release) = first_attempt_hold {
                // Either outcome ends the hold: a message, or a dropped sender.
                let _ = release.recv();
            }
            RootCommitProbe::Transient("stubbed held git probe".to_string())
        })
    })
}

#[cfg(test)]
pub(crate) fn force_git_root_commit_probe_commits_for_test(
    commits_by_root: BTreeMap<PathBuf, String>,
) -> GitRootCommitProbeOverrideGuard {
    install_git_root_commit_probe_override_for_test(move |project_root| {
        commits_by_root
            .get(project_root)
            .cloned()
            .map(RootCommitProbe::Commit)
    })
}

#[cfg(test)]
fn install_git_root_commit_probe_override_for_test(
    override_probe: impl Fn(&Path) -> Option<RootCommitProbe> + Send + Sync + 'static,
) -> GitRootCommitProbeOverrideGuard {
    let previous = set_git_root_commit_probe_override_for_test(Some(Arc::new(override_probe)));
    GitRootCommitProbeOverrideGuard { previous }
}

#[cfg(test)]
fn set_git_root_commit_probe_override_for_test(
    override_probe: Option<RootCommitProbeOverride>,
) -> Option<RootCommitProbeOverride> {
    let mut slot = GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::replace(&mut *slot, override_probe)
}

pub(crate) fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    crate::bounded_io::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;
    struct IsolatedLegacyMemoIndexes {
        indexes: Arc<Mutex<super::LegacyMemoIndexes>>,
        previous: Option<Arc<Mutex<super::LegacyMemoIndexes>>>,
    }

    impl Drop for IsolatedLegacyMemoIndexes {
        fn drop(&mut self) {
            super::LEGACY_MEMO_INDEXES_OVERRIDE.with(|slot| {
                *slot.borrow_mut() = self.previous.take();
            });
        }
    }

    // Isolate cache-policy tests from unrelated parallel tests' storage
    // roots while exercising the same slot-selection and lookup code.
    fn isolated_legacy_memo_indexes() -> IsolatedLegacyMemoIndexes {
        let indexes = Arc::new(Mutex::new(super::LegacyMemoIndexes::default()));
        install_legacy_memo_indexes(indexes)
    }

    fn install_legacy_memo_indexes(
        indexes: Arc<Mutex<super::LegacyMemoIndexes>>,
    ) -> IsolatedLegacyMemoIndexes {
        let previous = super::LEGACY_MEMO_INDEXES_OVERRIDE
            .with(|slot| slot.borrow_mut().replace(indexes.clone()));
        IsolatedLegacyMemoIndexes { indexes, previous }
    }

    #[test]
    fn legacy_identity_memo_retains_four_storage_roots_by_recency() {
        let cache = isolated_legacy_memo_indexes();
        let fixture = tempfile::tempdir().unwrap();
        let roots: Vec<_> = (0..5)
            .map(|index| {
                let storage = fixture.path().join(format!("storage-{index}"));
                fs::create_dir(&storage).unwrap();
                let query = storage.join("checkout");
                let memo = BTreeMap::from([(
                    query.to_string_lossy().into_owned(),
                    legacy_memo_entry(&format!("commit-{index}")),
                )]);
                fs::write(
                    super::artifact_cache_key_memo_path(&storage),
                    serde_json::to_vec(&memo).unwrap(),
                )
                .unwrap();
                (storage, query)
            })
            .collect();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        for (storage, query) in &roots[..4] {
            super::legacy_identity_memo(query, &query.to_string_lossy(), storage).unwrap();
        }
        let (storage, query) = &roots[0];
        super::legacy_identity_memo(query, &query.to_string_lossy(), storage).unwrap();
        let (storage, query) = &roots[4];
        super::legacy_identity_memo(query, &query.to_string_lossy(), storage).unwrap();
        assert_eq!(super::LEGACY_IDENTITY_MEMO_MAX_STORAGE_ROOTS, 4);
        let indexes = cache.indexes.lock().unwrap();
        assert_eq!(
            indexes.roots.len(),
            4,
            "five touched storage roots must retain only four indexes"
        );
        assert!(
            !indexes
                .roots
                .iter()
                .any(|(path, _)| path == &super::artifact_cache_key_memo_path(&roots[1].0)),
            "the refreshed first root must survive instead of the least recently used second root"
        );
        drop(indexes);
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 5);
        let (storage, query) = &roots[1];
        let actual = legacy_memo_value(super::legacy_identity_memo(
            query,
            &query.to_string_lossy(),
            storage,
        ));
        let expected = legacy_memo_value(super::legacy_identity_memo_full_scan(
            query,
            &query.to_string_lossy(),
            storage,
        ));
        assert_eq!(actual, expected);
        assert_eq!(
            super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
            6,
            "an evicted root must rebuild on its next lookup"
        );
        assert_eq!(cache.indexes.lock().unwrap().roots.len(), 4);
    }

    #[test]
    fn legacy_identity_memo_registry_touch_drops_expired_snapshot() {
        let cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let query = storage.path().join("checkout");
        let raw = query.to_string_lossy().into_owned();
        let memo = BTreeMap::from([(raw.clone(), legacy_memo_entry("root-commit"))]);
        fs::write(
            super::artifact_cache_key_memo_path(storage.path()),
            serde_json::to_vec(&memo).unwrap(),
        )
        .unwrap();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        let first = legacy_memo_value(super::legacy_identity_memo(&query, &raw, storage.path()));
        let slot = super::legacy_identity_memo_slot(storage.path());
        slot.lock().unwrap().as_mut().unwrap().built_at =
            Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE;
        let released = Arc::downgrade(&slot);
        drop(slot);
        super::legacy_identity_memo_slot(&storage.path().join("different-storage"));
        assert!(
            !cache
                .indexes
                .lock()
                .unwrap()
                .roots
                .iter()
                .any(|(path, _)| path == &super::artifact_cache_key_memo_path(storage.path())),
            "touching another root must remove an expired snapshot"
        );
        assert!(
            released.upgrade().is_none(),
            "the expired index must actually be released, not merely ignored"
        );
        assert_eq!(
            legacy_memo_value(super::legacy_identity_memo(&query, &raw, storage.path())),
            first
        );
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 2);
    }

    #[test]
    fn legacy_identity_memo_registry_touch_does_not_wait_for_busy_snapshot() {
        let cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let query = storage.path().join("checkout");
        let raw = query.to_string_lossy().into_owned();
        let memo = BTreeMap::from([(raw.clone(), legacy_memo_entry("root-commit"))]);
        fs::write(
            super::artifact_cache_key_memo_path(storage.path()),
            serde_json::to_vec(&memo).unwrap(),
        )
        .unwrap();
        super::legacy_identity_memo(&query, &raw, storage.path()).unwrap();
        let slot = super::legacy_identity_memo_slot(storage.path());
        slot.lock().unwrap().as_mut().unwrap().built_at =
            Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE;
        let held = slot.lock().unwrap();
        let shared = cache.indexes.clone();
        let other = storage.path().join("different-storage");
        let (finished, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _cache = install_legacy_memo_indexes(shared);
            super::legacy_identity_memo_slot(&other);
            finished.send(()).unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        assert!(
            result.is_ok(),
            "registry cleanup waited for a snapshot's filesystem-work lock"
        );
        assert!(
            !cache
                .indexes
                .lock()
                .unwrap()
                .roots
                .iter()
                .any(|(path, _)| path == &super::artifact_cache_key_memo_path(storage.path())),
            "even a busy expired snapshot must lose the registry's reference"
        );
    }

    fn legacy_memo_entry(commit: &str) -> super::ArtifactCacheKeyMemoEntry {
        super::ArtifactCacheKeyMemoEntry {
            key: "0123456789abcdef".into(),
            git_root_commit: commit.into(),
            recorded_at_ms: 1,
        }
    }

    fn legacy_memo_value(
        result: std::io::Result<Option<super::ArtifactCacheKeyMemoEntry>>,
    ) -> Result<Option<(String, String, u64)>, (std::io::ErrorKind, String)> {
        result
            .map(|entry| {
                entry.map(|entry| (entry.key, entry.git_root_commit, entry.recorded_at_ms))
            })
            .map_err(|error| (error.kind(), error.to_string()))
    }

    #[test]
    fn legacy_identity_memo_rebuilds_on_same_size_same_time_replacement() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "old"
        );
        entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
        let bytes = serde_json::to_vec_pretty(&entries).unwrap();
        crate::file_identity::replace_keeping_times(
            &super::artifact_cache_key_memo_path(storage.path()),
            &bytes,
        );
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "new"
        );
    }

    #[test]
    fn legacy_identity_memo_own_write_advances_snapshot() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.set(0));
        super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
        let slot = super::legacy_identity_memo_slot(storage.path());
        let built_at = slot.lock().unwrap().as_ref().unwrap().built_at;
        entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "new"
        );
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 1,
            "an own write must update the existing index rather than force the next root to rebuild it");
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().built_at,
            built_at,
            "own writes must not extend the age of unchanged canonical spellings"
        );
        assert_eq!(
            super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.get()),
            2
        );
        let added = storage.path().join("new-checkout");
        let added_raw = added.to_string_lossy().into_owned();
        entries.insert(added_raw.clone(), legacy_memo_entry("added"));
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(
            super::legacy_identity_memo(&added, &added_raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "added"
        );
        assert_eq!(
            super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.get()),
            3
        );
        entries.remove(&raw);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert!(super::legacy_identity_memo(&root, &raw, storage.path())
            .unwrap()
            .is_none());
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 1);
        assert_eq!(
            super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.get()),
            3,
            "removed and unchanged spellings need no canonicalization on own writes"
        );
    }

    #[test]
    fn legacy_identity_memo_own_write_rebuilds_uncertain_or_old_snapshot() {
        let _cache = isolated_legacy_memo_indexes();
        for reason in ["expired", "unknown", "external"] {
            let storage = tempfile::tempdir().unwrap();
            let root = storage.path().join("checkout");
            let raw = root.to_string_lossy().into_owned();
            let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
            super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
            let slot = super::legacy_identity_memo_slot(storage.path());
            match reason {
                "expired" => {
                    slot.lock().unwrap().as_mut().unwrap().built_at =
                        Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE
                }
                "unknown" => slot.lock().unwrap().as_mut().unwrap().stamp = None,
                _ => {
                    let external = BTreeMap::from([(raw.clone(), legacy_memo_entry("ext"))]);
                    crate::file_identity::replace_keeping_times(
                        &super::artifact_cache_key_memo_path(storage.path()),
                        &serde_json::to_vec_pretty(&external).unwrap(),
                    );
                }
            }
            entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            let current = super::legacy_identity_memo_slot(storage.path());
            assert!(
                current.lock().unwrap().is_none(),
                "{reason} snapshot must not be advanced"
            );
            if reason == "expired" {
                assert!(
                    !Arc::ptr_eq(&slot, &current),
                    "expiry removes the registry entry even while an old caller holds its slot"
                );
            }
            assert_eq!(
                super::legacy_identity_memo(&root, &raw, storage.path())
                    .unwrap()
                    .unwrap()
                    .git_root_commit,
                "new"
            );
            assert_eq!(
                super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
                2,
                "{reason}"
            );
        }
    }

    #[test]
    fn legacy_identity_memo_own_write_does_not_stamp_an_external_replacement() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
        let external = BTreeMap::from([(raw.clone(), legacy_memo_entry("ext"))]);
        let replacement = serde_json::to_vec_pretty(&external).unwrap();
        super::AFTER_LEGACY_MEMO_RENAME.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |path| {
                crate::file_identity::replace_keeping_times(path, &replacement);
            }))
        });
        entries.get_mut(&raw).unwrap().git_root_commit = "own".into();
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert!(super::legacy_identity_memo_slot(storage.path())
            .lock()
            .unwrap()
            .is_none());
        let indexed = super::legacy_identity_memo(&root, &raw, storage.path());
        assert_eq!(
            indexed.as_ref().unwrap().as_ref().unwrap().git_root_commit,
            "ext"
        );
        assert_eq!(
            legacy_memo_value(indexed),
            legacy_memo_value(super::legacy_identity_memo_full_scan(
                &root,
                &raw,
                storage.path()
            ))
        );
    }

    #[test]
    fn legacy_identity_memo_rechecks_in_place_edits_deletion_and_invalid_json() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let path = super::artifact_cache_key_memo_path(storage.path());
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
        fs::write(&path, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "new"
        );
        fs::remove_file(&path).unwrap();
        assert!(super::legacy_identity_memo(&root, &raw, storage.path())
            .unwrap()
            .is_none());
        fs::write(&path, b"not json").unwrap();
        assert_eq!(
            legacy_memo_value(super::legacy_identity_memo(&root, &raw, storage.path())),
            legacy_memo_value(super::legacy_identity_memo_full_scan(
                &root,
                &raw,
                storage.path()
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_identity_memo_revalidates_retargeted_matches() {
        let _cache = isolated_legacy_memo_indexes();
        for conflict in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let root = storage.path().join("checkout");
            let other = storage.path().join("other");
            fs::create_dir(&root).unwrap();
            fs::create_dir(&other).unwrap();
            let alias = storage.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            let raw = root.to_string_lossy().into_owned();
            let alias_raw = alias.to_string_lossy().into_owned();
            let mut entries =
                BTreeMap::from([(alias_raw.clone(), legacy_memo_entry("alias-commit"))]);
            if conflict {
                entries.insert(raw.clone(), legacy_memo_entry("root-commit"));
            }
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            let first = super::legacy_identity_memo(&root, &raw, storage.path());
            if conflict {
                assert!(first
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("conflicting repository identities"));
            } else {
                assert_eq!(first.unwrap().unwrap().git_root_commit, "alias-commit");
            }
            fs::remove_file(&alias).unwrap();
            std::os::unix::fs::symlink(&other, &alias).unwrap();
            let result = super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
            if conflict {
                assert_eq!(result.unwrap().git_root_commit, "root-commit");
            } else {
                assert!(
                    result.is_none(),
                    "retargeted alias must not return its former identity"
                );
                assert!(
                    super::legacy_identity_memo(&root, &alias_raw, storage.path())
                        .unwrap()
                        .is_none(),
                    "raw-spelling shortcut must not bypass revalidation"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_identity_memo_age_refresh_discovers_new_aliases() {
        let _cache = isolated_legacy_memo_indexes();
        for conflict in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let root = storage.path().join("checkout");
            fs::create_dir(&root).unwrap();
            let alias = storage.path().join("formerly-missing");
            let raw = root.to_string_lossy().into_owned();
            let mut entries = BTreeMap::from([(
                alias.to_string_lossy().into_owned(),
                legacy_memo_entry("root-commit"),
            )]);
            if conflict {
                entries.insert(raw.clone(), legacy_memo_entry("other-commit"));
            }
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            let before =
                legacy_memo_value(super::legacy_identity_memo(&root, &raw, storage.path()));
            assert_eq!(
                before,
                legacy_memo_value(super::legacy_identity_memo_full_scan(
                    &root,
                    &raw,
                    storage.path()
                ))
            );
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert_eq!(legacy_memo_value(super::legacy_identity_memo(&root, &raw, storage.path())), before,
                "new aliases and conflicts are discovered at refresh rather than by rescanning every miss");
            let slot = super::legacy_identity_memo_slot(storage.path());
            slot.lock().unwrap().as_mut().unwrap().built_at =
                Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE;
            let refreshed = super::legacy_identity_memo(&root, &raw, storage.path());
            if conflict {
                assert!(refreshed
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("conflicting repository identities"));
            } else {
                assert_eq!(refreshed.unwrap().unwrap().git_root_commit, "root-commit");
            }
        }
    }

    #[test]
    fn legacy_identity_memo_index_matches_randomized_full_scan() {
        let _cache = isolated_legacy_memo_indexes();
        fn next(seed: &mut u64) -> u64 {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *seed
        }
        let storage = tempfile::tempdir().unwrap();
        let roots: Vec<_> = (0..9)
            .map(|index| {
                let root = storage.path().join(format!("checkout-{index}"));
                fs::create_dir(&root).unwrap();
                root
            })
            .collect();
        let mut queries = roots.clone();
        let mut spellings = Vec::new();
        for (index, root) in roots[..8].iter().enumerate() {
            spellings.push(root.clone());
            spellings.push(root.join("."));
            let missing = storage.path().join(format!("gone-{index}/nested/project"));
            queries.push(missing.clone());
            spellings.push(missing);
            #[cfg(unix)]
            {
                let alias = storage.path().join(format!("symlink-{index}"));
                std::os::unix::fs::symlink(root, &alias).unwrap();
                spellings.push(alias.clone());
                spellings.push(alias.join("missing/project"));
                queries.push(root.join("missing/project"));
            }
        }
        let mut seed = 0x615f_726f_6f74_u64;
        for round in 0..32 {
            let mut entries = BTreeMap::new();
            for _ in 0..48 {
                let spelling = &spellings[next(&mut seed) as usize % spellings.len()];
                let mut entry = legacy_memo_entry(&format!("root-{}", next(&mut seed) % 3));
                entry.recorded_at_ms = next(&mut seed);
                entry.key = format!("{:016x}", next(&mut seed));
                if next(&mut seed) % 11 == 0 {
                    entry.key = "invalid".into();
                }
                entries.insert(spelling.to_string_lossy().into_owned(), entry);
            }
            let fixed = roots[8].to_string_lossy().into_owned();
            let alias = roots[8].join(".").to_string_lossy().into_owned();
            entries.insert(fixed.clone(), legacy_memo_entry("fixed-root"));
            let mut second = legacy_memo_entry("fixed-root");
            second.key = "fedcba9876543210".into();
            match round % 4 {
                0 => second.git_root_commit = "conflicting-root".into(),
                1 => {}
                2 => second.git_root_commit = " ".into(),
                _ => second.key = "invalid".into(),
            }
            entries.insert(alias.clone(), second);
            for write in 0..4 {
                let selected = spellings[next(&mut seed) as usize % spellings.len()]
                    .to_string_lossy()
                    .into_owned();
                match write {
                    0 => {}
                    1 => {
                        entries.insert(selected, legacy_memo_entry("changed-root"));
                    }
                    2 => {
                        entries.remove(&selected);
                    }
                    _ => {
                        entries.insert(alias.clone(), legacy_memo_entry("fixed-root"));
                    }
                }
                super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
                for root in &queries {
                    let raw = root.to_string_lossy();
                    let indexed =
                        legacy_memo_value(super::legacy_identity_memo(root, &raw, storage.path()));
                    let scanned = legacy_memo_value(super::legacy_identity_memo_full_scan(
                        root,
                        &raw,
                        storage.path(),
                    ));
                    assert_eq!(
                        indexed, scanned,
                        "round={round} write={write} root={root:?}"
                    );
                }
                let unrelated = storage.path().join("unrelated-query");
                assert_eq!(
                    legacy_memo_value(super::legacy_identity_memo(
                        &unrelated,
                        &fixed,
                        storage.path()
                    )),
                    legacy_memo_value(super::legacy_identity_memo_full_scan(
                        &unrelated,
                        &fixed,
                        storage.path()
                    )),
                    "exact raw spellings must retain the full scan's shortcut"
                );
            }
        }
    }

    #[test]
    fn legacy_identity_memo_repeated_misses_share_snapshot() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("new-checkout");
        let entries = BTreeMap::from([(
            storage
                .path()
                .join("removed/checkouts/old")
                .to_string_lossy()
                .into_owned(),
            super::ArtifactCacheKeyMemoEntry {
                key: "0123456789abcdef".into(),
                git_root_commit: "root-commit".into(),
                recorded_at_ms: 1,
            },
        )]);
        fs::write(
            super::artifact_cache_key_memo_path(storage.path()),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        for _ in 0..2 {
            assert!(
                super::legacy_identity_memo(&root, "new-checkout", storage.path())
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
            1,
            "unchanged memo misses must share the canonical-alias snapshot"
        );
    }

    // Frozen from c71c33a96, including its SHA-256 and path/session key formulas.
    // Git probing and memo codecs are shared, but no record-aware resolver or
    // current top-level/path key hasher may supply the compatibility expectation.
    fn base_c71_cache_key(
        probe_root: &Path,
        memo_root: &Path,
        storage_root: &Path,
        git_common_dir: Option<&Path>,
        timings: &mut ArtifactCacheKeyTimings,
    ) -> Result<String, ArtifactCacheKeyProbeError> {
        fn base_hash16(bytes: &[u8]) -> String {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            format!("{:x}", hasher.finalize())[..16].to_string()
        }
        fn artifact_path_identity_key(root: &Path) -> String {
            base_hash16(canonicalize_or_normalize(root).to_string_lossy().as_bytes())
        }
        fn session_artifact_key_from_git_identity(root: &Path, root_commit: &str) -> String {
            match checkout_position(root) {
                CheckoutPosition::TopLevel => base_hash16(root_commit.as_bytes()),
                CheckoutPosition::Subdirectory(relative) => {
                    match crate::scoped_key::scoped_v1_key(root_commit, relative.as_bytes()) {
                        Ok(scoped) => scoped[..16].to_string(),
                        Err(_) => artifact_path_identity_key(root),
                    }
                }
                CheckoutPosition::Unrepresentable => artifact_path_identity_key(root),
            }
        }
        let memo_root_key = artifact_cache_key_memo_root_key(memo_root);
        let git_marker_state = root_git_marker_state(probe_root, git_common_dir);
        if git_marker_state == GitMarkerState::Absent {
            return Ok(artifact_path_identity_key(probe_root));
        }

        match repo_root_commit_with_retry_profiled(probe_root, timings) {
            RootCommitResolution::Commit(root_commit) => {
                let key = session_artifact_key_from_git_identity(probe_root, &root_commit);
                record_derived_cache_key(memo_root, &key);
                if let Err(error) = record_artifact_cache_key_memo_profiled(
                    storage_root,
                    &memo_root_key,
                    &key,
                    &root_commit,
                    timings,
                ) {
                    crate::slog_warn!(
                        "artifact cache key: failed to persist memo for {} in {}: {}",
                        memo_root.display(),
                        storage_root.display(),
                        error
                    );
                }
                Ok(key)
            }
            RootCommitResolution::NotARepo => Ok(artifact_path_identity_key(probe_root)),
            RootCommitResolution::Failed(detail) => {
                if let Some(entry) =
                    lookup_artifact_cache_key_memo_profiled(storage_root, &memo_root_key, timings)
                {
                    // Re-derive from the memoized repository identity rather than
                    // trusting the memoized key: memos written by older versions
                    // hold the whole-repository key even for subfolder roots, and
                    // reusing it would let a subfolder session overwrite the
                    // repository's shared artifact again.
                    let key =
                        session_artifact_key_from_git_identity(probe_root, &entry.git_root_commit);
                    crate::slog_warn!(
                        "artifact cache key: probe failed, using memoized identity (key {}) for {}",
                        key,
                        memo_root.display()
                    );
                    return Ok(key);
                }

                match git_marker_state {
                    GitMarkerState::Absent => Ok(artifact_path_identity_key(probe_root)),
                    GitMarkerState::Present => Err(ArtifactCacheKeyProbeError {
                        root: memo_root.to_path_buf(),
                        detail,
                        failure: ArtifactCacheKeyFailure::GitUnavailable,
                    }),
                    GitMarkerState::Unknown(marker_detail) => Err(ArtifactCacheKeyProbeError {
                        root: memo_root.to_path_buf(),
                        detail: format!("{detail}; {marker_detail}"),
                        failure: ArtifactCacheKeyFailure::GitUnavailable,
                    }),
                }
            }
            RootCommitResolution::Cancelled => Err(ArtifactCacheKeyProbeError {
                root: memo_root.to_path_buf(),
                detail: "artifact cache key probe cancelled".to_string(),
                failure: ArtifactCacheKeyFailure::IdentityUnavailable,
            }),
        }
    }

    fn upgrade_fixture_git(
        root: &Path,
        args: &[&str],
        author: &serde_json::Value,
    ) -> std::process::Output {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(root));
        let output = command
            .args(args)
            .env("GIT_AUTHOR_NAME", author["name"].as_str().unwrap())
            .env("GIT_COMMITTER_NAME", author["name"].as_str().unwrap())
            .env("GIT_AUTHOR_EMAIL", author["email"].as_str().unwrap())
            .env("GIT_COMMITTER_EMAIL", author["email"].as_str().unwrap())
            .env("GIT_AUTHOR_DATE", author["date"].as_str().unwrap())
            .env("GIT_COMMITTER_DATE", author["date"].as_str().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn assert_upgrade_identity_fixture(name: &str, repeat_bind: bool) {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(
            crate::developer_tools::git_usable(),
            "upgrade fixtures require usable Git"
        );
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"].as_str() == Some(name))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        for (relative, content) in fixture["files"].as_object().unwrap() {
            let path = repository.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content.as_str().unwrap()).unwrap();
        }
        let plain = case["layout"] == "plain";
        let author = &fixture["commit"];
        if !plain {
            upgrade_fixture_git(&repository, &["init", "-q"], author);
            upgrade_fixture_git(&repository, &["add", "."], author);
            upgrade_fixture_git(
                &repository,
                &["commit", "-q", "-m", author["message"].as_str().unwrap()],
                author,
            );
        }
        let root = if case["layout"] == "linked_worktree" {
            let linked = directory.path().join("linked");
            upgrade_fixture_git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    linked.to_str().unwrap(),
                    "HEAD",
                ],
                author,
            );
            assert!(linked.join(".git").is_file());
            linked
        } else {
            repository.clone()
        };
        let root = fs::canonicalize(root).unwrap();
        if plain {
            assert!(
                root.ancestors().all(|parent| !parent.join(".git").exists()),
                "plain fixture must be outside any repository"
            );
        } else if case["layout"] == "repository" {
            assert!(root.join(".git").is_dir());
        }
        let common = (!plain).then(|| fs::canonicalize(repository.join(".git")).unwrap());
        let base_storage = directory.path().join("base-storage");
        let upgraded_storage = directory.path().join("upgraded-storage");
        if case["legacy_memo"] == true {
            let seed = base_c71_cache_key(
                &root,
                &root,
                &directory.path().join("seed-storage"),
                common.as_deref(),
                &mut ArtifactCacheKeyTimings::default(),
            )
            .unwrap();
            let output =
                upgrade_fixture_git(&root, &["rev-list", "--max-parents=0", "HEAD"], author);
            let entry = ArtifactCacheKeyMemoEntry {
                key: seed,
                git_root_commit: String::from_utf8(output.stdout).unwrap().trim().to_string(),
                recorded_at_ms: current_time_millis(),
            };
            for storage in [&base_storage, &upgraded_storage] {
                write_cache_key_memo(
                    storage,
                    &BTreeMap::from([(artifact_cache_key_memo_root_key(&root), entry.clone())]),
                );
            }
            assert_eq!(
                fs::read(base_storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap(),
                fs::read(upgraded_storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap()
            );
        }
        // Separate stores keep the old oracle's memo writes out of the upgrade input.
        let expected = base_c71_cache_key(
            &root,
            &root,
            &base_storage,
            common.as_deref(),
            &mut ArtifactCacheKeyTimings::default(),
        )
        .unwrap();
        assert!(!base_storage.join("root-identities").exists());
        assert!(crate::root_identity::read(&root, &upgraded_storage)
            .unwrap()
            .is_none());
        let actual =
            artifact_cache_key_with_memo(&root, &root, &upgraded_storage, common.as_deref())
                .unwrap();
        assert_eq!(
            actual, expected,
            "first upgrade bind changed the base key for {name}"
        );
        assert!(crate::root_identity::read(&root, &upgraded_storage)
            .unwrap()
            .is_some());
        if repeat_bind {
            assert!(matches!(
                crate::root_identity::read(&root, &upgraded_storage).unwrap(),
                Some(crate::root_identity::Identity::Git { .. })
            ));
            let next =
                artifact_cache_key_with_memo(&root, &root, &upgraded_storage, common.as_deref())
                    .unwrap();
            assert_eq!(
                next, expected,
                "record-derived key changed on the next bind"
            );
        }
    }

    #[test]
    fn upgrade_identity_repository_top_level_matches_base() {
        assert_upgrade_identity_fixture("repository_top_level", false);
    }

    #[test]
    fn upgrade_identity_linked_worktree_matches_base() {
        assert_upgrade_identity_fixture("linked_worktree", false);
    }

    #[test]
    fn upgrade_identity_plain_folder_matches_base() {
        assert_upgrade_identity_fixture("plain_folder", false);
    }

    #[test]
    fn upgrade_identity_legacy_git_memo_matches_base() {
        assert_upgrade_identity_fixture("legacy_git_memo", false);
    }

    #[test]
    fn upgrade_identity_written_repository_record_keeps_base_key() {
        assert_upgrade_identity_fixture("repository_top_level", true);
    }

    /// A repository with a corrupt HEAD makes Git print its ordinary "not a git
    /// repository" message. That is damaged metadata, not proof of absence, so
    /// the root-commit probe must stay retryable instead of choosing the
    /// path-derived cache identity reserved for real non-repositories.
    #[test]
    fn corrupt_root_commit_metadata_is_not_path_identity_fallback() {
        assert!(crate::developer_tools::git_usable());
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::init_repository_with_corrupt_head_for_test(dir.path());
        assert!(matches!(
            git_root_commit_once_real(dir.path(), &mut ArtifactCacheKeyTimings::default()),
            RootCommitProbe::Transient(_)
        ));
    }

    /// A `.git` folder without HEAD (for example only `info/exclude`) is not a
    /// repository to Git, so it keeps the path-derived identity of a plain folder.
    #[test]
    fn stub_git_directory_root_commit_is_not_a_repository() {
        assert!(crate::developer_tools::git_usable());
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::write_stub_git_directory_for_test(dir.path());
        assert!(matches!(
            git_root_commit_once_real(dir.path(), &mut ArtifactCacheKeyTimings::default()),
            RootCommitProbe::NotARepo
        ));
        assert!(matches!(
            repo_root_commit_with_retry(dir.path()),
            RootCommitResolution::NotARepo
        ));
    }

    /// A verified repository without commits keeps its intentional mapping to
    /// the path-derived identity.
    #[test]
    fn unborn_root_commit_retains_the_intentional_non_repository_mapping() {
        assert!(crate::developer_tools::git_usable());
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success());
        let mut timings = ArtifactCacheKeyTimings::default();
        assert!(matches!(
            git_root_commit_once_real(dir.path(), &mut timings),
            RootCommitProbe::NoCommit
        ));
        // rev-list, then git-dir, verify, symbolic-ref and two git-path checks.
        assert_eq!(timings.git_spawns, 6);
        assert!(matches!(
            repo_root_commit_with_retry(dir.path()),
            RootCommitResolution::NotARepo
        ));
    }

    pub(crate) fn git_command_for_test(root: &Path) -> Command {
        let mut command = Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.arg("-C").arg(root));
        command
    }

    fn grafted_history_test_roots() -> [&'static str; 3] {
        [
            "7e96b9e0000000000000000000000000000000",
            "1e394c20000000000000000000000000000000",
            "40587520000000000000000000000000000000",
        ]
    }

    fn artifact_key_from_root_commit_output(stdout: &[u8]) -> String {
        match canonicalize_root_commit_output(stdout) {
            RootCommitProbe::Commit(root_commit) => artifact_key_from_git_identity(&root_commit),
            RootCommitProbe::NoCommit => panic!("root commit output unexpectedly empty"),
            RootCommitProbe::NotARepo => panic!("root commit output was not a repository"),
            RootCommitProbe::Transient(detail) => panic!("root commit output failed: {detail}"),
        }
    }

    fn force_git_root_commit_probe_outputs_for_test(
        outputs_by_root: BTreeMap<PathBuf, Vec<u8>>,
    ) -> GitRootCommitProbeOverrideGuard {
        install_git_root_commit_probe_override_for_test(move |project_root| {
            outputs_by_root
                .get(project_root)
                .map(|stdout| canonicalize_root_commit_output(stdout))
        })
    }

    #[test]
    fn artifact_cache_key_canonicalizes_all_root_permutations() {
        let roots = grafted_history_test_roots();
        let mut sorted_roots = roots;
        sorted_roots.sort_unstable();
        let expected_commit = sorted_roots.join("\n");
        let expected_key = artifact_key_from_git_identity(&expected_commit);

        let permutations = [
            [roots[0], roots[1], roots[2]],
            [roots[0], roots[2], roots[1]],
            [roots[1], roots[0], roots[2]],
            [roots[1], roots[2], roots[0]],
            [roots[2], roots[0], roots[1]],
            [roots[2], roots[1], roots[0]],
        ];
        for permutation in permutations {
            let output = format!("{}\n", permutation.join("\n"));
            let RootCommitProbe::Commit(canonical) =
                canonicalize_root_commit_output(output.as_bytes())
            else {
                panic!("root permutation did not produce a commit");
            };
            assert_eq!(canonical, expected_commit);
            assert_eq!(artifact_key_from_git_identity(&canonical), expected_key);
        }
    }

    #[test]
    fn artifact_cache_key_deduplicates_repeated_roots() {
        let root = grafted_history_test_roots()[0];
        let one_root = format!("{root}\n");
        let duplicate_root = format!("{root}\n{root}\n");

        assert_eq!(
            artifact_key_from_root_commit_output(one_root.as_bytes()),
            artifact_key_from_root_commit_output(duplicate_root.as_bytes())
        );
    }

    #[test]
    fn artifact_cache_key_ignores_blank_whitespace_and_crlf_lines() {
        let roots = grafted_history_test_roots();
        let clean = format!("{}\n{}\n", roots[0], roots[1]);
        let decorated = format!(" \r\n\t{}  \r\n{}\t\r\n\r\n", roots[1], roots[0]);

        assert_eq!(
            artifact_key_from_root_commit_output(clean.as_bytes()),
            artifact_key_from_root_commit_output(decorated.as_bytes())
        );
    }

    #[test]
    fn artifact_cache_key_single_root_matches_the_old_trimmed_derivation() {
        let root = grafted_history_test_roots()[0];
        let output = format!("{root}\n");
        let old_trimmed = String::from_utf8_lossy(output.as_bytes())
            .trim()
            .to_string();
        let old_key = artifact_hash16(old_trimmed.as_bytes());

        let RootCommitProbe::Commit(canonical) = canonicalize_root_commit_output(output.as_bytes())
        else {
            panic!("single root output did not produce a commit");
        };
        assert_eq!(canonical, root);
        assert_eq!(artifact_key_from_git_identity(&canonical), old_key);
    }

    #[test]
    fn artifact_cache_key_empty_canonical_root_output_is_no_commit() {
        assert!(matches!(
            canonicalize_root_commit_output(b" \r\n\t\n\r\n"),
            RootCommitProbe::NoCommit
        ));
    }

    #[test]
    fn artifact_cache_key_memo_round_trip_uses_canonical_root_set() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let roots = grafted_history_test_roots();
        let mut sorted_roots = roots;
        sorted_roots.sort_unstable();
        let canonical = sorted_roots.join("\n");
        let expected_key = artifact_key_from_git_identity(&canonical);
        let first_output = format!("{}\n{}\n{}\n", roots[2], roots[0], roots[1]);
        let second_output = format!("{}\n{}\n{}\n", roots[1], roots[2], roots[0]);

        {
            let mut outputs = BTreeMap::new();
            outputs.insert(root.clone(), first_output.into_bytes());
            let _override = force_git_root_commit_probe_outputs_for_test(outputs);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None)
                    .expect("first canonical key"),
                expected_key
            );
        }
        let first_memo_bytes =
            fs::read(artifact_cache_key_memo_path(&storage)).expect("read first memo bytes");

        {
            let mut outputs = BTreeMap::new();
            outputs.insert(root.clone(), second_output.into_bytes());
            let _override = force_git_root_commit_probe_outputs_for_test(outputs);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None)
                    .expect("second canonical key"),
                expected_key
            );
        }
        let second_memo_bytes =
            fs::read(artifact_cache_key_memo_path(&storage)).expect("read second memo bytes");
        assert_eq!(
            first_memo_bytes, second_memo_bytes,
            "an unchanged canonical memo entry should not be rewritten"
        );
        let memo = read_cache_key_memo(&storage);
        assert_eq!(
            memo.get(root.to_string_lossy().as_ref())
                .expect("canonical memo entry")
                .git_root_commit,
            canonical
        );
    }

    #[test]
    fn artifact_cache_key_replaces_old_unsorted_memo_entry() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let roots = grafted_history_test_roots();
        let unsorted = format!("{}\n{}", roots[2], roots[0]);
        let mut sorted_roots = [roots[2], roots[0]];
        sorted_roots.sort_unstable();
        let canonical = sorted_roots.join("\n");
        let expected_key = artifact_key_from_git_identity(&canonical);

        let mut seeded_memo = BTreeMap::new();
        seeded_memo.insert(
            root.to_string_lossy().into_owned(),
            ArtifactCacheKeyMemoEntry {
                key: artifact_key_from_git_identity(&unsorted),
                git_root_commit: unsorted.clone(),
                recorded_at_ms: 1,
            },
        );
        fs::create_dir_all(&storage).expect("create memo storage");
        fs::write(
            artifact_cache_key_memo_path(&storage),
            serde_json::to_vec_pretty(&seeded_memo).expect("serialize seeded memo"),
        )
        .expect("write seeded memo");

        let mut outputs = BTreeMap::new();
        outputs.insert(root.clone(), unsorted.into_bytes());
        let _override = force_git_root_commit_probe_outputs_for_test(outputs);
        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("canonical probe should replace old memo");

        assert_eq!(key, expected_key);
        let memo = read_cache_key_memo(&storage);
        let entry = memo
            .get(root.to_string_lossy().as_ref())
            .expect("replaced memo entry");
        assert_eq!(entry.key, expected_key);
        assert_eq!(entry.git_root_commit, canonical);
    }

    #[test]
    fn artifact_cache_key_reuses_root_commit_until_head_moves() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        // No scripted probe may be installed meanwhile: it bypasses the cache.
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");
        let git = |args: &[&str]| {
            assert!(git_command_for_test(&root)
                .args([
                    "-c",
                    "user.name=AFT Tests",
                    "-c",
                    "user.email=aft@example.com"
                ])
                .args(args)
                .status()
                .expect("run git")
                .success());
        };
        git(&["init", "--quiet"]);
        fs::write(root.join("tracked.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "first"]);

        // Many derivations of an unchanged root (the call-graph store does
        // one per refresh) run git once.
        let first = artifact_cache_key(&root);
        for _ in 0..9 {
            assert_eq!(artifact_cache_key(&root), first);
        }
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // Staging rewrites .git/index through index.lock, as git status
        // refreshes do. HEAD is unchanged, so git is not asked again.
        fs::write(root.join("tracked.txt"), "two\n").unwrap();
        git(&["add", "tracked.txt"]);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // A new commit moves HEAD: git runs again, the root set is unchanged.
        git(&["commit", "--quiet", "-am", "second"]);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        let second = git_command_for_test(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let second = String::from_utf8(second.stdout).unwrap().trim().to_string();

        // An unrelated history checked out under HEAD must change the key.
        git(&["checkout", "--quiet", "--orphan", "other"]);
        git(&["commit", "--quiet", "-m", "unrelated root"]);
        let other = artifact_cache_key(&root);
        assert_ne!(other, first, "a different root commit kept the old key");
        assert_eq!(root_commit_git_runs_for_test(&root), 3);

        // Detaching HEAD at the second commit, whose history has the first
        // root, brings back the first key; the stamp sees the HEAD change.
        git(&["checkout", "--quiet", "--detach", &second]);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 4);
    }

    fn root_commit_test_git(root: &Path, args: &[&str]) -> String {
        let output = git_command_for_test(root)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft@example.com",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn root_commit_cache_does_not_store_a_probe_across_history_changes() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "first root"]);
        let first = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--orphan", "other"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "unrelated root"]);
        let other = git(&["rev-parse", "HEAD"]);
        assert_ne!(first, other);
        git(&["checkout", "--quiet", "--detach", &first]);

        let resolution = repo_root_commit_with_probe(&root, || {
            // Move to an unrelated history after the first stamp but before git
            // reads HEAD, leaving the second stamp to observe the new history.
            git(&["checkout", "--quiet", "--detach", &other]);
            repo_root_commit_with_retry_profiled(&root, &mut ArtifactCacheKeyTimings::default())
        });
        assert!(matches!(resolution, RootCommitResolution::Commit(commit) if commit == other));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);
        assert!(
            !root_commit_cache().lock().unwrap().contains_key(&root),
            "a probe spanning different repository stamps must not be cached"
        );

        // Returning to the first stamp must not reuse the unrelated root.
        git(&["checkout", "--quiet", "--detach", &first]);
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == first));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        let _ = repo_root_commit_with_retry(&root);
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[test]
    fn root_commit_cache_tracks_replace_ref_configuration() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original root"]);
        let original_root = git(&["rev-parse", "HEAD"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "child"]);
        let head = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "--quiet", "--orphan", "replacement"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement root",
        ]);
        let replacement = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--detach", &head]);
        git(&["replace", &head, &replacement]);
        git(&["config", "core.useReplaceRefs", "false"]);

        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == original_root));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // The replacement makes HEAD a root only when Git honors replace refs.
        git(&["config", "core.useReplaceRefs", "true"]);
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[test]
    fn root_commit_cache_bypasses_an_external_config_include() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let external_config_dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let external_config = external_config_dir.path().join("included.gitconfig");
        let external_config_arg = external_config.to_str().unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original root"]);
        let original_root = git(&["rev-parse", "HEAD"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "child"]);
        let head = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "--quiet", "--orphan", "replacement"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement root",
        ]);
        let replacement = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--detach", &head]);
        git(&["replace", &head, &replacement]);
        git(&["config", "--local", "include.path", external_config_arg]);
        let set_external_setting = |value: &str| {
            let output = git_command_for_test(&root)
                .args([
                    "config",
                    "--file",
                    external_config_arg,
                    "core.useReplaceRefs",
                    value,
                ])
                .output()
                .expect("run git config for the included file");
            assert!(output.status.success(), "{output:?}");
        };

        set_external_setting("false");
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == original_root));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        set_external_setting("true");
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[test]
    fn root_commit_cache_tracks_retargeted_loose_replace_refs() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original root"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original child"]);
        let head = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "--quiet", "--orphan", "replacement"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement root",
        ]);
        let replacement_root = git(&["rev-parse", "HEAD"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement child",
        ]);
        let replacement_child = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--detach", &head]);
        git(&["replace", &head, &replacement_root]);

        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // Retargeting this same loose ref changes its bytes, not the directory entries.
        let replace_ref = root.join(".git/refs/replace").join(&head);
        fs::write(&replace_ref, format!("{replacement_child}\n")).unwrap();
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == replacement_root));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[cfg(unix)]
    #[test]
    fn root_commit_cache_bypasses_symlinked_grafts() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "first"]);
        let first = git(&["rev-parse", "HEAD"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "second"]);
        let second = git(&["rev-parse", "HEAD"]);
        let shared = root.join("shared-grafts");
        fs::write(&shared, "").unwrap();
        std::os::unix::fs::symlink(&shared, root.join(".git/info/grafts")).unwrap();

        assert_eq!(
            artifact_cache_key(&root),
            artifact_key_from_git_identity(&first)
        );
        // Only the target changes: the graft turns the second commit into a root.
        fs::write(&shared, format!("{second}\n")).unwrap();
        assert_eq!(
            artifact_cache_key(&root),
            artifact_key_from_git_identity(&second)
        );
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        assert_eq!(
            artifact_cache_key(&root),
            artifact_key_from_git_identity(&second)
        );
        assert_eq!(root_commit_git_runs_for_test(&root), 3);
        assert!(!root_commit_cache().lock().unwrap().contains_key(&root));
    }

    #[test]
    fn artifact_cache_key_shared_across_clones_of_same_repo() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let source = dir.path().join("source");
        fs::create_dir_all(&source).expect("create source repo dir");
        fs::write(source.join("tracked.txt"), "content\n").expect("write tracked file");

        let mut init = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(init.current_dir(&source))
                .args(["init"])
                .status()
                .expect("init git repo")
                .success()
        );
        assert!(git_command_for_test(&source)
            .args(["add", "."])
            .status()
            .expect("git add")
            .success());
        assert!(git_command_for_test(&source)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "-m",
                "initial",
            ])
            .status()
            .expect("git commit")
            .success());

        let clone = dir.path().join("clone");
        let mut clone_command = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&source)
            .arg(&clone)
            .status()
            .expect("git clone")
            .success());

        let source_key = artifact_cache_key(&source);
        let clone_key = artifact_cache_key(&clone);

        assert_eq!(source_key.len(), 16);
        assert_eq!(clone_key.len(), 16);
        // Same repo (same root commit) → same cache key regardless of clone path
        assert_eq!(source_key, clone_key);
    }

    fn read_cache_key_memo(storage_root: &Path) -> BTreeMap<String, ArtifactCacheKeyMemoEntry> {
        let bytes = fs::read(artifact_cache_key_memo_path(storage_root)).expect("read memo file");
        serde_json::from_slice(&bytes).expect("parse memo file")
    }

    fn write_cache_key_memo(
        storage_root: &Path,
        entries: &BTreeMap<String, ArtifactCacheKeyMemoEntry>,
    ) {
        fs::create_dir_all(storage_root).expect("create memo storage");
        fs::write(
            artifact_cache_key_memo_path(storage_root),
            serde_json::to_vec_pretty(entries).expect("serialize memo"),
        )
        .expect("write memo");
    }

    fn git_like_root(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        fs::create_dir_all(root.join(".git")).expect("create git marker");
        root
    }

    /// `git pack-refs` rewrites `packed-refs` by atomic replacement. When the
    /// current branch is packed and moves to another 40-character commit, the
    /// file keeps its length; a rewrite within one clock tick also keeps every
    /// time, so only the file identity shows that the root commit may differ.
    #[test]
    fn root_commit_stamp_sees_a_same_length_packed_refs_replacement() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let directory = tempfile::tempdir().unwrap();
        let root = git_like_root(&directory, "repository");
        let git = root.join(".git");
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let packed = |commit: char| format!("{} refs/heads/main\n", commit.to_string().repeat(40));
        fs::write(git.join("packed-refs"), packed('a')).unwrap();
        let before = RootCommitStamp::capture(&root).expect("the fixture must allow caching");
        crate::file_identity::replace_keeping_times(
            &git.join("packed-refs"),
            packed('b').as_bytes(),
        );
        assert_ne!(RootCommitStamp::capture(&root), Some(before));
    }

    #[test]
    fn artifact_cache_key_memo_write_prunes_only_deleted_old_entries() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let live_old_root = dir.path().join("live-old");
        let dead_old_root = dir.path().join("dead-old");
        let dead_recent_root = dir.path().join("dead-recent");
        let written_root = dir.path().join("written");
        fs::create_dir_all(&live_old_root).expect("create live root");
        fs::create_dir_all(&written_root).expect("create written root");
        let now = current_time_millis();
        let old = now.saturating_sub(ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64 + 1);
        let recent =
            now.saturating_sub(ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64 / 2);
        let mut seeded = BTreeMap::new();
        for (root, key, recorded_at_ms) in [
            (&live_old_root, "1111111111111111", old),
            (&dead_old_root, "2222222222222222", old),
            (&dead_recent_root, "3333333333333333", recent),
        ] {
            seeded.insert(
                root.to_string_lossy().into_owned(),
                ArtifactCacheKeyMemoEntry {
                    key: key.to_string(),
                    git_root_commit: "fixture-commit".to_string(),
                    recorded_at_ms,
                },
            );
        }
        write_cache_key_memo(&storage, &seeded);

        record_artifact_cache_key_memo(
            &storage,
            written_root.to_string_lossy().as_ref(),
            "4444444444444444",
            "written-commit",
        )
        .expect("record memo entry");

        let memo = read_cache_key_memo(&storage);
        assert!(memo.contains_key(live_old_root.to_string_lossy().as_ref()));
        assert!(!memo.contains_key(dead_old_root.to_string_lossy().as_ref()));
        assert!(memo.contains_key(dead_recent_root.to_string_lossy().as_ref()));
        assert!(memo.contains_key(written_root.to_string_lossy().as_ref()));
    }

    #[test]
    fn artifact_cache_key_memo_prunes_hundreds_of_deleted_entries_on_next_write() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let written_root = dir.path().join("written");
        fs::create_dir_all(&written_root).expect("create written root");
        let old = current_time_millis()
            .saturating_sub(ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64 + 1);
        let mut seeded = BTreeMap::new();
        for index in 0..400 {
            seeded.insert(
                dir.path()
                    .join(format!("dead-{index}"))
                    .to_string_lossy()
                    .into_owned(),
                ArtifactCacheKeyMemoEntry {
                    key: format!("{index:016x}"),
                    git_root_commit: format!("fixture-commit-{index}"),
                    recorded_at_ms: old,
                },
            );
        }
        write_cache_key_memo(&storage, &seeded);
        let bytes_before = fs::metadata(artifact_cache_key_memo_path(&storage))
            .expect("stat seeded memo")
            .len();

        record_artifact_cache_key_memo(
            &storage,
            written_root.to_string_lossy().as_ref(),
            "aaaaaaaaaaaaaaaa",
            "written-commit",
        )
        .expect("record memo entry");

        let memo = read_cache_key_memo(&storage);
        let bytes_after = fs::metadata(artifact_cache_key_memo_path(&storage))
            .expect("stat pruned memo")
            .len();
        assert_eq!(
            memo.len(),
            1,
            "next write must remove all stale fixture roots"
        );
        assert!(memo.contains_key(written_root.to_string_lossy().as_ref()));
        assert!(
            bytes_after < bytes_before,
            "pruning hundreds of dead entries must shrink the memo file"
        );
    }

    #[test]
    fn artifact_cache_key_memo_read_hit_refreshes_existing_root_once_per_day() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = dir.path().join("borrowed-root");
        fs::create_dir_all(&root).expect("create borrowed root");
        let mut seeded = BTreeMap::new();
        seeded.insert(
            root.to_string_lossy().into_owned(),
            ArtifactCacheKeyMemoEntry {
                key: "aaaaaaaaaaaaaaaa".to_string(),
                git_root_commit: "fixture-commit".to_string(),
                recorded_at_ms: 0,
            },
        );
        write_cache_key_memo(&storage, &seeded);

        let first = lookup_artifact_cache_key_memo(&storage, root.to_string_lossy().as_ref())
            .expect("memo hit");
        let persisted_first = read_cache_key_memo(&storage)
            .get(root.to_string_lossy().as_ref())
            .expect("persisted refreshed entry")
            .recorded_at_ms;
        let second = lookup_artifact_cache_key_memo(&storage, root.to_string_lossy().as_ref())
            .expect("second memo hit");
        let persisted_second = read_cache_key_memo(&storage)
            .get(root.to_string_lossy().as_ref())
            .expect("persisted entry after second hit")
            .recorded_at_ms;

        assert!(first.recorded_at_ms > 0);
        assert_eq!(first.recorded_at_ms, persisted_first);
        assert_eq!(second.recorded_at_ms, persisted_second);
        assert_eq!(persisted_first, persisted_second);
    }

    #[test]
    fn artifact_cache_key_success_writes_memo() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let mut commits = BTreeMap::new();
        commits.insert(root.clone(), commit.clone());
        let _override = force_git_root_commit_probe_commits_for_test(commits);

        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("cache key from successful probe");

        assert_eq!(key, artifact_key_from_git_identity(&commit));
        let memo = read_cache_key_memo(&storage);
        let entry = memo
            .get(root.to_string_lossy().as_ref())
            .expect("memo entry for root");
        assert_eq!(entry.key, key);
        assert_eq!(entry.git_root_commit, commit);
        assert!(entry.recorded_at_ms > 0);
    }

    #[test]
    fn artifact_cache_key_probe_failure_uses_memoized_key() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let expected_key = artifact_key_from_git_identity(&commit);
        {
            let mut commits = BTreeMap::new();
            commits.insert(root.clone(), commit);
            let _override = force_git_root_commit_probe_commits_for_test(commits);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).expect("initial key"),
                expected_key
            );
        }
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let rescued = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("memo should rescue transient probe failure");

        assert_eq!(rescued, expected_key);
        assert_ne!(rescued, artifact_path_identity_key(&root));
    }

    #[test]
    fn subfolder_session_key_is_scoped_and_shared_by_the_same_subfolder_of_every_checkout() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let home = git_like_root(&dir, "home");
        // A linked worktree's top level holds a `.git` file, not a directory.
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&worktree).expect("create worktree root");
        fs::write(worktree.join(".git"), "gitdir: elsewhere\n").expect("write .git file");
        let mut roots = Vec::new();
        for root in [&home, &worktree] {
            for sub in ["", "sub1", "sub1/deeper", "sub2"] {
                let path = if sub.is_empty() {
                    root.clone()
                } else {
                    root.join(sub)
                };
                fs::create_dir_all(&path).expect("create session root");
                roots.push(path);
            }
        }
        let commit = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let _override = force_git_root_commit_probe_commits_for_test(
            roots
                .iter()
                .map(|root| (root.clone(), commit.clone()))
                .collect(),
        );

        let repository_key = artifact_key_from_git_identity(&commit);
        let key = |path: &Path| artifact_cache_key(path);
        assert_eq!(key(&home), repository_key);
        assert_eq!(key(&worktree), repository_key);
        let sub1 = key(&home.join("sub1"));
        let deeper = key(&home.join("sub1/deeper"));
        let sub2 = key(&home.join("sub2"));
        for scoped in [&sub1, &deeper, &sub2] {
            assert_ne!(
                scoped, &repository_key,
                "a subfolder must not publish the repository artifact"
            );
            assert!(
                artifact_key_looks_valid(scoped),
                "session storage needs the 16-hex shape"
            );
        }
        assert_ne!(sub1, sub2);
        assert_ne!(sub1, deeper);
        assert_eq!(
            sub1,
            crate::scoped_key::scoped_v1_key(&commit, b"sub1").unwrap()[..16]
        );
        assert_eq!(key(&worktree.join("sub1")), sub1);
        assert_eq!(key(&worktree.join("sub1/deeper")), deeper);
        assert_eq!(key(&worktree.join("sub2")), sub2);
    }

    #[test]
    fn subfolder_probe_failure_ignores_a_repository_key_memoized_by_older_versions() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let home = git_like_root(&dir, "home");
        let sub = home.join("sub1");
        fs::create_dir_all(&sub).expect("create subfolder");
        let commit = "dddddddddddddddddddddddddddddddddddddddd".to_string();
        let repository_key = artifact_key_from_git_identity(&commit);
        let mut seeded = BTreeMap::new();
        seeded.insert(
            sub.to_string_lossy().into_owned(),
            ArtifactCacheKeyMemoEntry {
                key: repository_key.clone(),
                git_root_commit: commit.clone(),
                recorded_at_ms: current_time_millis(),
            },
        );
        write_cache_key_memo(&storage, &seeded);
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![sub.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let rescued = artifact_cache_key_with_memo(&sub, &sub, &storage, Some(&home.join(".git")))
            .expect("memo should rescue transient probe failure");

        assert_ne!(rescued, repository_key);
        assert_eq!(
            rescued,
            crate::scoped_key::scoped_v1_key(&commit, b"sub1").unwrap()[..16]
        );
    }

    #[test]
    fn checkout_position_finds_the_nearest_git_marker() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let home = git_like_root(&dir, "home");
        let nested = home.join("a/b");
        let submodule = home.join("vendor/lib");
        fs::create_dir_all(&nested).expect("create nested dir");
        fs::create_dir_all(submodule.join("src")).expect("create submodule dir");
        fs::write(submodule.join(".git"), "gitdir: ../../.git/modules/lib\n")
            .expect("write submodule marker");

        assert_eq!(checkout_position(&home), CheckoutPosition::TopLevel);
        assert_eq!(
            checkout_position(&nested),
            CheckoutPosition::Subdirectory("a/b".to_string())
        );
        assert_eq!(checkout_position(&submodule), CheckoutPosition::TopLevel);
        assert_eq!(
            checkout_position(&submodule.join("src")),
            CheckoutPosition::Subdirectory("src".to_string())
        );
    }

    #[test]
    fn artifact_cache_key_probe_failure_without_memo_rejects_git_like_root() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let error = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect_err("git-like root without memo must not use path identity");

        assert_eq!(error.root(), root.as_path());
        assert!(error.detail().contains("Too many open files"));
    }

    #[test]
    fn artifact_cache_key_probe_failure_without_git_marker_uses_path_identity() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = dir.path().join("plain");
        fs::create_dir_all(&root).expect("create non-git root");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("non-git root keeps legacy path identity fallback");

        assert_eq!(key, artifact_path_identity_key(&root));
    }

    #[test]
    fn gitless_first_bind_records_sticky_path_identity() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let expected = artifact_path_identity_key(&root);
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected
            );
        });
        assert!(fs::read_dir(storage.join("root-identities"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")));
        let mut commits = BTreeMap::new();
        commits.insert(
            root.clone(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        );
        let _probe = force_git_root_commit_probe_commits_for_test(commits);
        crate::developer_tools::with_git_usable_for_test(true, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected,
                "installing Git must not retag a path-bound root"
            );
            let standing = crate::scoped_key::resolve_standing_root_in_storage(
                root.to_str().unwrap(),
                &storage,
                true,
            )
            .unwrap();
            assert_eq!(standing.artifact_key, expected);
            assert_eq!(standing.resolved_git_toplevel, None);
        });
    }

    #[test]
    fn usable_git_rebind_replaces_an_unrelated_repository_identity() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkout");
        let replacement = directory.path().join("replacement");
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        for (path, content) in [
            (&root, "first repository"),
            (&replacement, "unrelated repository"),
        ] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("lib.rs"), content).unwrap();
            upgrade_fixture_git(path, &["init", "-q"], &fixture["commit"]);
            upgrade_fixture_git(path, &["add", "."], &fixture["commit"]);
            upgrade_fixture_git(path, &["commit", "-q", "-m", content], &fixture["commit"]);
        }
        let storage = directory.path().join("storage");
        let first = artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap();
        let expected = base_c71_cache_key(
            &replacement,
            &replacement,
            &directory.path().join("base"),
            None,
            &mut ArtifactCacheKeyTimings::default(),
        )
        .unwrap();
        assert_ne!(
            first, expected,
            "the fixtures must be unrelated repositories"
        );
        fs::rename(&root, directory.path().join("archived-checkout")).unwrap();
        fs::rename(&replacement, &root).unwrap();
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                first
            );
        });
        assert_eq!(
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
            expected,
            "usable Git must not reuse the replaced repository's key"
        );
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected,
                "the replacement identity must be durably recorded"
            );
        });
        fs::rename(&root, directory.path().join("second-archived-checkout")).unwrap();
        fs::create_dir(&root).unwrap();
        upgrade_fixture_git(&root, &["init", "-q"], &fixture["commit"]);
        let unborn = base_c71_cache_key(
            &root,
            &root,
            &directory.path().join("unborn-base"),
            None,
            &mut ArtifactCacheKeyTimings::default(),
        )
        .unwrap();
        assert_ne!(unborn, expected);
        assert_eq!(
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
            unborn,
            "a verified unborn replacement must not reuse the legacy repository key"
        );
    }

    #[test]
    fn recorded_repository_identity_survives_gitless_restart() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let expected = artifact_key_from_git_identity(&commit);
        write_cache_key_memo(
            &storage,
            &BTreeMap::from([(
                root.parent()
                    .unwrap()
                    .join(".")
                    .join("repo")
                    .to_string_lossy()
                    .into_owned(),
                ArtifactCacheKeyMemoEntry {
                    key: expected.clone(),
                    git_root_commit: commit,
                    recorded_at_ms: current_time_millis(),
                },
            )]),
        );
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected
            );
            fs::remove_file(storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap();
            artifact_cache_key_memo_state()
                .lock()
                .unwrap()
                .by_storage_root
                .remove(&storage);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected,
                "the durable repository identity must survive without the legacy memo"
            );
            assert_ne!(expected, artifact_path_identity_key(&root));
            let standing = crate::scoped_key::resolve_standing_root_in_storage(
                root.to_str().unwrap(),
                &storage,
                true,
            )
            .unwrap();
            assert_eq!(standing.artifact_key, expected);
            assert!(standing.resolved_git_toplevel.is_some());
        });
    }

    #[test]
    fn plain_standing_root_resolves_without_usable_git() {
        let root = tempfile::tempdir().unwrap();
        crate::developer_tools::with_git_usable_for_test(false, || {
            let resolved =
                crate::scoped_key::resolve_standing_root(root.path().to_str().unwrap()).unwrap();
            assert_eq!(
                resolved.artifact_key,
                artifact_path_identity_key(root.path())
            );
            assert_eq!(resolved.resolved_git_toplevel, None);
        });
        crate::developer_tools::with_git_usable_for_test(true, || {
            crate::git_probe::with_child_env_for_test(
                &[("PATH", std::ffi::OsStr::new(""))],
                || {
                    assert!(
                        crate::scoped_key::resolve_standing_root(root.path().to_str().unwrap())
                            .is_ok(),
                        "a plain folder must resolve even when any Git spawn would fail"
                    );
                },
            );
        });
    }

    #[test]
    fn saved_identity_survives_uncertain_probe_after_legacy_memo_pruning() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = git_like_root(&directory, "repository");
        let storage = directory.path().join("storage");
        let saved = {
            let _probe = force_git_root_commit_probe_commits_for_test(BTreeMap::from([(
                root.clone(),
                "b".repeat(40),
            )]));
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap()
        };
        assert!(matches!(
            crate::root_identity::read(&root, &storage).unwrap(),
            Some(crate::root_identity::Identity::Git { .. })
        ));
        fs::remove_file(storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap();
        artifact_cache_key_memo_state()
            .lock()
            .unwrap()
            .by_storage_root
            .remove(&storage);
        let _probe = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "temporary Git probe failure",
        );
        assert_eq!(
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
            saved,
            "an uncertain probe must keep the saved identity without a legacy memo"
        );
        assert!(!storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE).exists());
    }

    fn certification_fixture_repository(root: &Path, content: &str, author: &serde_json::Value) {
        fs::create_dir(root).unwrap();
        fs::write(root.join("lib.rs"), content).unwrap();
        upgrade_fixture_git(root, &["init", "-q"], author);
        upgrade_fixture_git(root, &["add", "."], author);
        upgrade_fixture_git(root, &["commit", "-q", "-m", content], author);
    }

    fn certification_standing_config(root: &Path, storage: &Path) -> crate::config::Config {
        crate::config::Config {
            storage_dir: Some(storage.to_path_buf()),
            index: crate::config::IndexConfig {
                roots: vec![crate::config::IndexRootConfig {
                    path: root.display().to_string(),
                    indexes: vec![crate::config::IndexKind::Search],
                }],
            },
            ..crate::config::Config::default()
        }
    }

    #[test]
    fn unmodeled_history_never_reuses_a_standing_identity_stamp() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository");
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        certification_fixture_repository(&root, "initial history", &fixture["commit"]);
        let output = upgrade_fixture_git(
            &root,
            &["commit-tree", "HEAD^{tree}", "-m", "unrelated history"],
            &fixture["commit"],
        );
        let replacement = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let included = directory.path().join("included.gitconfig");
        fs::write(&included, "[core]\nlogAllRefUpdates = true\n").unwrap();
        upgrade_fixture_git(
            &root,
            &["config", "include.path", included.to_str().unwrap()],
            &fixture["commit"],
        );
        let root = fs::canonicalize(root).unwrap();
        assert!(
            RootCommitStamp::capture(&root).is_none(),
            "the fixture must decline history caching"
        );
        let standing = crate::standing_roots::StandingRoots::default();
        let config = certification_standing_config(&root, &directory.path().join("storage"));
        let old = standing.reconcile(&config).unwrap().active_entries[0]
            .artifact_key
            .clone();
        let expected = artifact_key_from_git_identity(&replacement);
        assert_ne!(old, expected);
        let marker_before = RootCommitFileIdentity::of(&root.join(".git"), true);
        let head_before = RootCommitFileIdentity::of(&root.join(".git/HEAD"), false);
        let branch = upgrade_fixture_git(&root, &["symbolic-ref", "HEAD"], &fixture["commit"]);
        let branch = String::from_utf8(branch.stdout).unwrap();
        fs::write(
            root.join(".git").join(branch.trim()),
            format!("{replacement}\n"),
        )
        .unwrap();
        assert_eq!(
            RootCommitFileIdentity::of(&root.join(".git"), true),
            marker_before
        );
        assert_eq!(
            RootCommitFileIdentity::of(&root.join(".git/HEAD"), false),
            head_before
        );
        let before = root_commit_git_runs_for_test(&root);
        let new = standing
            .reconcile_background(&config)
            .unwrap()
            .active_entries[0]
            .artifact_key
            .clone();
        assert_eq!(
            new, expected,
            "an unmodeled branch-ref change must re-verify identity"
        );
        assert!(root_commit_git_runs_for_test(&root) > before);
        let before = root_commit_git_runs_for_test(&root);
        standing.reconcile_background(&config).unwrap();
        assert!(
            root_commit_git_runs_for_test(&root) > before,
            "unmodeled history must verify on every pass"
        );
    }

    #[test]
    fn failed_replacement_probe_does_not_certify_the_replacement_stamp() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkout");
        let replacement = directory.path().join("replacement");
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        certification_fixture_repository(&root, "first checkout", &fixture["commit"]);
        certification_fixture_repository(&replacement, "second checkout", &fixture["commit"]);
        let replacement_commit =
            upgrade_fixture_git(&replacement, &["rev-parse", "HEAD"], &fixture["commit"]);
        let expected = artifact_key_from_git_identity(
            String::from_utf8(replacement_commit.stdout).unwrap().trim(),
        );
        let root = fs::canonicalize(root).unwrap();
        let config = certification_standing_config(&root, &directory.path().join("storage"));
        let standing = crate::standing_roots::StandingRoots::default();
        let old = standing.reconcile(&config).unwrap().active_entries[0]
            .artifact_key
            .clone();
        assert_ne!(old, expected);
        fs::rename(&root, directory.path().join("archived-checkout")).unwrap();
        fs::rename(&replacement, &root).unwrap();
        {
            let _probe = force_git_root_commit_probe_transient_for_paths_for_test(
                vec![root.clone()],
                "temporary replacement probe failure",
            );
            assert_eq!(
                standing
                    .reconcile_background(&config)
                    .unwrap()
                    .active_entries[0]
                    .artifact_key,
                old
            );
        }
        let before = root_commit_git_runs_for_test(&root);
        let recovered = standing
            .reconcile_background(&config)
            .unwrap()
            .active_entries[0]
            .artifact_key
            .clone();
        assert_eq!(
            recovered, expected,
            "a failed check must not certify the replacement's metadata"
        );
        assert!(root_commit_git_runs_for_test(&root) > before);
    }

    #[test]
    fn unchanged_standing_passes_skip_git_identity_probes() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        for linked in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let repository = directory.path().join("repository");
            fs::create_dir(&repository).unwrap();
            fs::write(repository.join("lib.rs"), "pub fn standing_fixture() {}\n").unwrap();
            upgrade_fixture_git(&repository, &["init", "-q"], &fixture["commit"]);
            upgrade_fixture_git(&repository, &["add", "."], &fixture["commit"]);
            upgrade_fixture_git(
                &repository,
                &["commit", "-q", "-m", "standing fixture"],
                &fixture["commit"],
            );
            let root = if linked {
                let worktree = directory.path().join("worktree");
                upgrade_fixture_git(
                    &repository,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        worktree.to_str().unwrap(),
                        "HEAD",
                    ],
                    &fixture["commit"],
                );
                worktree
            } else {
                repository
            };
            let root = fs::canonicalize(root).unwrap();
            let config = crate::config::Config {
                storage_dir: Some(directory.path().join("storage")),
                index: crate::config::IndexConfig {
                    roots: vec![crate::config::IndexRootConfig {
                        path: root.display().to_string(),
                        indexes: vec![crate::config::IndexKind::Search],
                    }],
                },
                ..crate::config::Config::default()
            };
            let standing = crate::standing_roots::StandingRoots::default();
            standing.reconcile(&config).unwrap();
            assert!(
                matches!(
                    crate::root_identity::read(&root, config.storage_dir.as_deref().unwrap())
                        .unwrap(),
                    Some(crate::root_identity::Identity::Git { .. })
                ),
                "fixture must record a Git identity (linked: {linked})"
            );
            let first = root_commit_git_runs_for_test(&root);
            assert!(
                first > 0,
                "the counter must observe the initial real Git probe"
            );
            for _ in 0..8 {
                standing.reconcile_background(&config).unwrap();
            }
            assert_eq!(
                root_commit_git_runs_for_test(&root) - first,
                0,
                "unchanged standing roots must not spawn Git (linked: {linked})"
            );
            let output = upgrade_fixture_git(
                &root,
                &["rev-parse", "--git-path", "HEAD"],
                &fixture["commit"],
            );
            let head = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
            let head = if head.is_absolute() {
                head
            } else {
                root.join(head)
            };
            let before_head_change = repository_identity_stamp(&root);
            filetime::set_file_mtime(head, filetime::FileTime::from_unix_time(60, 0)).unwrap();
            assert_ne!(
                repository_identity_stamp(&root),
                before_head_change,
                "HEAD mtime must change repository evidence (linked: {linked})"
            );
            standing.reconcile_background(&config).unwrap();
            assert_eq!(
                root_commit_git_runs_for_test(&root) - first,
                1,
                "a changed HEAD stamp must recheck Git"
            );
            standing.reconcile_background(&config).unwrap();
            assert_eq!(root_commit_git_runs_for_test(&root) - first, 1);
            standing.reconcile(&config).unwrap();
            assert_eq!(
                root_commit_git_runs_for_test(&root) - first,
                2,
                "a real bind must still verify Git"
            );
        }
    }

    #[test]
    fn artifact_cache_key_corrupt_memo_is_absent_not_a_path_identity_escape() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        fs::create_dir_all(&storage).expect("create storage root");
        fs::write(artifact_cache_key_memo_path(&storage), b"not json").expect("write corrupt memo");
        let root = git_like_root(&dir, "repo");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let error = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect_err("corrupt memo is treated as absent");

        assert!(error.detail().contains("Too many open files"));
    }

    #[test]
    fn artifact_cache_key_concurrent_memo_writes_keep_valid_json() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root_a = git_like_root(&dir, "repo-a");
        let root_b = git_like_root(&dir, "repo-b");
        let commit_a = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let commit_b = "dddddddddddddddddddddddddddddddddddddddd".to_string();
        let mut commits = BTreeMap::new();
        commits.insert(root_a.clone(), commit_a.clone());
        commits.insert(root_b.clone(), commit_b.clone());
        let _override = force_git_root_commit_probe_commits_for_test(commits);

        let storage_a = storage.clone();
        let thread_a = std::thread::spawn({
            let root_a = root_a.clone();
            move || artifact_cache_key_with_memo(&root_a, &root_a, &storage_a, None)
        });
        let storage_b = storage.clone();
        let thread_b = std::thread::spawn({
            let root_b = root_b.clone();
            move || artifact_cache_key_with_memo(&root_b, &root_b, &storage_b, None)
        });

        let key_a = thread_a.join().expect("join writer a").expect("key a");
        let key_b = thread_b.join().expect("join writer b").expect("key b");
        let memo = read_cache_key_memo(&storage);

        assert_eq!(
            memo.get(root_a.to_string_lossy().as_ref())
                .expect("root a memo")
                .key,
            key_a
        );
        assert_eq!(
            memo.get(root_b.to_string_lossy().as_ref())
                .expect("root b memo")
                .key,
            key_b
        );
        assert_eq!(key_a, artifact_key_from_git_identity(&commit_a));
        assert_eq!(key_b, artifact_key_from_git_identity(&commit_b));
    }
}
