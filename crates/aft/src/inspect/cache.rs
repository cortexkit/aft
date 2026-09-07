use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INSPECT_SQLITE_SIDECAR_SUFFIXES: &[&str] = &["-wal", "-shm", "-journal"];
const INSPECT_WRITER_LEASE_TIMEOUT: Duration = Duration::from_secs(2);
// Field data (2026-08-06): ~/.local/share/cortexkit/aft/inspect held 1,945
// scope-key directories for about 30 repositories; 1,849 directories (21.5 GB)
// had no file newer than seven days. Scope keys hash canonical checkout paths,
// so reclaimed worktrees cannot ever reach their old directories again. This is
// a structural fix for that permanent accumulation, not a user-facing knob.
const INSPECT_SCOPE_MIN_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Bound one process-wide pass so a slow filesystem cannot stall publication;
/// the first-level cursor resumes the remaining scope directories next time.
const INSPECT_SCOPE_SWEEP_BUDGET: Duration = Duration::from_secs(5);

#[derive(Default)]
struct InspectScopeSweepCursor {
    last_name: Option<String>,
}

static INSPECT_SCOPE_SWEEP_CURSORS: OnceLock<Mutex<HashMap<PathBuf, InspectScopeSweepCursor>>> =
    OnceLock::new();

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::cache_freshness::{FileFreshness, FreshnessVerdict};
use crate::config::Config;
use crate::db::{SqliteStore, TrackedConnection};
use crate::jsonc::strip_jsonc;

use super::job::{
    contribution_with_type_ref_names, type_ref_names_from_contribution, FileContribution,
    InspectCategory, JobKey,
};

#[derive(Debug, Default)]
pub(crate) struct Tier2ContributionUpdates {
    pub upserts: Vec<FileContribution>,
    pub deletes: Vec<PathBuf>,
    pub metadata_updates: Vec<(PathBuf, FileFreshness)>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InspectDbTimings {
    pub lock_wait: Duration,
    pub transaction: Duration,
}

#[derive(Debug)]
pub enum InspectCacheError {
    Io(std::io::Error),
    WriterLeaseTimeout,
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    LockPoisoned(&'static str),
    InvalidHash(String),
}

impl fmt::Display for InspectCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InspectCacheError::Io(error) => write!(formatter, "inspect cache io error: {error}"),
            InspectCacheError::WriterLeaseTimeout => {
                write!(
                    formatter,
                    "writer_lease_timeout: inspect writer lease deadline elapsed"
                )
            }
            InspectCacheError::Sql(error) => {
                write!(formatter, "inspect cache sqlite error: {error}")
            }
            InspectCacheError::Json(error) => {
                write!(formatter, "inspect cache json error: {error}")
            }
            InspectCacheError::LockPoisoned(name) => {
                write!(formatter, "inspect cache lock poisoned: {name}")
            }
            InspectCacheError::InvalidHash(hash) => {
                write!(formatter, "inspect cache invalid blake3 hash: {hash}")
            }
        }
    }
}

impl std::error::Error for InspectCacheError {}

impl From<std::io::Error> for InspectCacheError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for InspectCacheError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<serde_json::Error> for InspectCacheError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Persisted Tier-2 contribution/aggregate format version.
///
/// Bump this when `FileContribution.contribution` JSON changes in a way that
/// requires existing per-file contributions to be rebuilt before roll-up, OR
/// when the roll-up/aggregation LOGIC changes (e.g. dead_code reachability):
/// cached aggregates are keyed by a `contribution_set_hash` that folds in this
/// version, so a logic-only change is invisible to existing caches unless the
/// version moves. v6: dead_code now propagates liveness through dispatch-only
/// method bodies (free fns reached only via `obj.method()` were false-dead).
/// v7: duplicates now collapses nested/overlapping fragments (a duplicated
/// block no longer reports every nested subtree as its own group).
/// v8: entry-point recognition seeds npm `scripts` source files as liveness
/// roots (baked into per-file liveness_roots), and dead_code/unused_exports
/// exclude test-support files (fixtures/corpora/mocks) from reporting.
/// v9: unused_exports resolves NodeNext `./x.js` import specifiers to their
/// `.ts` source (alters resolved import edges), fixing false-unused on symbols
/// re-exported/imported with a `.js` extension in a `.ts` source tree.
/// v10: public-API entry resolution remaps build-output entries (dist/index.js)
/// to their src/ source equivalent, so the source barrel is recognized as a
/// public-API file and its re-exports are suppressed (changes public-API set).
/// v11: dead_code/unused_exports drill-down is ranked by signal tier (product
/// findings before benchmark/tooling noise) before the cap, and a ranked `top`
/// preview is folded into all three Tier-2 aggregates — changes cached payload.
/// v12: dead_code internal call rows include call-edge provenance, changing
/// cached per-file contribution payloads and aggregate roll-up inputs.
/// v13: dead_code callgraph snapshots are projected from the persisted
/// CallgraphStore; per-row provenance now reflects store resolution tiers.
/// v14: TS/JS dead_code and unused_exports contributions carry oxc verdicts,
/// provenance, and oxc honesty metadata.
/// v15: dead_code reachability counts exact type_match call edges as resolved
/// liveness (qualified-constructor calls like AppContext::new -> BackupStore::new
/// no longer collapse to bare `new` and drop), changing the dead verdict for the
/// same contribution set — existing caches must invalidate.
/// v16: unused_exports stores raw oxc FileFacts and recomputes verdicts during
/// roll-up, enabling incremental one-file reparses without stale verdicts.
/// v17: dead_code stores raw per-file facts and recomputes callgraph/re-export,
/// entry-root, imported-export, and oxc verdict liveness during roll-up.
/// v18: dead_code/unused_exports aggregate hashes include the full TS/JS
/// resolver-config dependency set (tsconfig/jsconfig variants and extends
/// chains), so alias-only config edits invalidate verdict roll-ups.
/// v19: dead_code entry reachability executes side-effect-only imported modules,
/// preserving same-file and transitive static-import liveness without marking all
/// target exports used.
/// v20: duplicates aggregate hashes include inspect.duplicates.expected_mirrors,
/// so changing intentional mirror architecture rules invalidates cached roll-ups.
/// v21: Rust dead_code contributions carry attribute-root entry facts for
/// externally-invoked functions such as Tauri commands and ABI exports.
/// v22: cycles persists TS/JS resolved import-edge facts and rolls them up into
/// strongly connected module components.
/// v23: TS/JS re-export verdicts collapse barrel aliases to canonical exports
/// and carry non-counted re-export context on canonical findings.
/// v24: TS/JS verdicts distinguish product references from test-only
/// references so test-only usage moves out of headline dead/unused counts.
/// v25: framework file-based route files execute as roots while only their
/// framework-called exports are seeded live; manifest-driven route detection
/// changes dead/unused verdict roll-ups without changing per-file facts.
/// v26: TS/JS per-file facts record exported-symbol decorators, and NestJS
/// decorator roots change dead/unused verdict roll-ups.
/// v28: Rust dead-code facts include function/type names found inside macro
/// token trees. The aggregate scan uses those names for reachability only;
/// call-graph navigation remains based on resolved source-level calls.
/// v29: duplicates verdicts require every reported occurrence to span at least
/// 10 source lines, so unchanged per-file fragment facts can now aggregate to a
/// different surfaced group set.
/// v30: cycles verdicts retain singleton strongly connected components when a
/// resolved non-type import explicitly points back to the same file.
/// v31: TODO extraction now reads parser comment nodes and ignores marker-like
/// text in strings, changing aggregate verdicts for unchanged source files.
/// v32: Rust dead-code reachability follows function items referenced as values,
/// and callers inside cfg(test) regions move otherwise-dead targets into the
/// test-only bucket.
/// v33: cyclomatic-complexity contributions and thresholded hotspot aggregates
/// add a Tier-2 category, so unchanged contribution sets need a new roll-up.
/// v34: Rust macro-token calls and declared-module edges change dead-code
/// reachability and test-only verdicts for unchanged contribution sets.
pub(crate) const TIER2_CONTRIBUTION_CACHE_VERSION: u32 = 34;

#[derive(Debug, Clone)]
pub struct ContributionRecord {
    pub category: InspectCategory,
    pub file_path: PathBuf,
    pub freshness: FileFreshness,
    pub contribution: serde_json::Value,
    pub type_ref_names: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct MemoryAggregate {
    payload: serde_json::Value,
    generated_at: i64,
    contribution_set_hash: Option<String>,
}

const TIER1_FILE_MEMO_MAX_ENTRIES: usize = 4_096;

#[derive(Debug, Clone)]
struct Tier1MemoEntry<T> {
    freshness: FileFreshness,
    value: T,
    generation: u64,
}

#[derive(Debug, Clone)]
struct LruNode {
    path: PathBuf,
    generation: u64,
}

#[derive(Debug)]
struct Tier1MemoState<T> {
    entries: HashMap<PathBuf, Tier1MemoEntry<T>>,
    lru: VecDeque<LruNode>,
    next_generation: u64,
    capacity: usize,
}

impl<T> Default for Tier1MemoState<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            next_generation: 0,
            capacity: TIER1_FILE_MEMO_MAX_ENTRIES,
        }
    }
}

impl<T> Tier1MemoState<T> {
    fn insert(&mut self, path: PathBuf, mut entry: Tier1MemoEntry<T>) {
        let generation = self.allocate_generation();
        entry.generation = generation;
        self.entries.insert(path.clone(), entry);
        self.lru.push_back(LruNode { path, generation });
        self.compact_lru_if_needed();
        self.evict_lru();
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
        self.compact_lru_if_needed();
    }

    fn touch(&mut self, path: &Path) {
        if !self.entries.contains_key(path) {
            return;
        }

        let generation = self.allocate_generation();
        if let Some(entry) = self.entries.get_mut(path) {
            entry.generation = generation;
            self.lru.push_back(LruNode {
                path: path.to_path_buf(),
                generation,
            });
        }
        self.compact_lru_if_needed();
    }

    fn allocate_generation(&mut self) -> u64 {
        if self.next_generation == u64::MAX {
            self.rebuild_lru();
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        generation
    }

    fn compact_lru_if_needed(&mut self) {
        let max_lru_nodes = self.capacity.saturating_mul(2).max(self.entries.len());
        if self.lru.len() > max_lru_nodes {
            self.rebuild_lru();
        }
    }

    fn rebuild_lru(&mut self) {
        let mut live_nodes = self
            .entries
            .iter()
            .map(|(path, entry)| (entry.generation, path.clone()))
            .collect::<Vec<_>>();
        live_nodes.sort_by_key(|(generation, _)| *generation);

        self.lru.clear();
        for (generation, (_, path)) in live_nodes.into_iter().enumerate() {
            let generation = generation as u64;
            if let Some(entry) = self.entries.get_mut(&path) {
                entry.generation = generation;
            }
            self.lru.push_back(LruNode { path, generation });
        }
        self.next_generation = self.lru.len() as u64;
    }

    fn retain_live_lru_nodes(&mut self) {
        let entries = &self.entries;
        self.lru.retain(|node| {
            entries
                .get(&node.path)
                .is_some_and(|entry| entry.generation == node.generation)
        });
    }

    fn evict_lru(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(node) = self.lru.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&node.path)
                .is_some_and(|entry| entry.generation == node.generation)
            {
                self.entries.remove(&node.path);
            }
        }
        self.compact_lru_if_needed();
    }
}

#[derive(Debug)]
pub(crate) struct Tier1FileMemo<T> {
    state: Mutex<Tier1MemoState<T>>,
}

impl<T> Default for Tier1FileMemo<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(Tier1MemoState::default()),
        }
    }
}

impl<T> Tier1FileMemo<T> {
    /// Prevent a full-scope scan from evicting entries that the same scan will
    /// need again on its next run. Capacity only grows here; a completed full
    /// scan shrinks it back to that scan's live file set in [`Self::prune_to_scope`].
    pub(crate) fn reserve_for_scan(&self, file_count: usize) {
        let required_capacity = file_count.max(TIER1_FILE_MEMO_MAX_ENTRIES);
        if let Ok(mut state) = self.state.lock() {
            state.capacity = state.capacity.max(required_capacity);
        }
    }

    /// Drop entries outside a completed full-project scan and right-size the
    /// memo to the live project. Scoped scans must not call this because their
    /// path list intentionally omits valid entries elsewhere in the project.
    pub(crate) fn prune_to_scope(&self, project_root: &Path, live_paths: &[PathBuf]) {
        let live_paths = live_paths
            .iter()
            .map(PathBuf::as_path)
            .collect::<HashSet<_>>();
        if let Ok(mut state) = self.state.lock() {
            state.entries.retain(|path, _| {
                !path.starts_with(project_root) || live_paths.contains(path.as_path())
            });
            state.capacity = live_paths.len().max(TIER1_FILE_MEMO_MAX_ENTRIES);
            state.retain_live_lru_nodes();
            state.evict_lru();
        }
    }
}

impl<T: Clone> Tier1FileMemo<T> {
    pub(crate) fn get_or_insert_with<F>(&self, path: &Path, scan: F) -> T
    where
        F: FnOnce(&Path) -> (Option<FileFreshness>, T),
    {
        if let Some(cached) = self.cached_value(path) {
            return cached;
        }

        let (freshness, value) = scan(path);
        if let Ok(mut state) = self.state.lock() {
            if let Some(freshness) = freshness {
                state.insert(
                    path.to_path_buf(),
                    Tier1MemoEntry {
                        freshness,
                        value: value.clone(),
                        generation: 0,
                    },
                );
            } else {
                state.remove(path);
            }
        }
        value
    }

    fn cached_value(&self, path: &Path) -> Option<T> {
        let mut cached = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.entries.get(path).cloned())?;

        match crate::cache_freshness::verify_file(path, &cached.freshness) {
            FreshnessVerdict::HotFresh => {
                if let Ok(mut state) = self.state.lock() {
                    state.touch(path);
                }
                Some(cached.value)
            }
            FreshnessVerdict::ContentFresh {
                new_mtime,
                new_size,
            } => {
                cached.freshness.mtime = new_mtime;
                cached.freshness.size = new_size;
                let value = cached.value.clone();
                if let Ok(mut state) = self.state.lock() {
                    state.insert(path.to_path_buf(), cached);
                }
                Some(value)
            }
            FreshnessVerdict::Stale => None,
            FreshnessVerdict::Deleted => {
                if let Ok(mut state) = self.state.lock() {
                    state.remove(path);
                }
                None
            }
        }
    }
}

#[derive(Debug)]
pub struct InspectCache {
    project_root: PathBuf,
    project_key: String,
    sqlite_path: PathBuf,
    writer_lease: Option<Arc<crate::root_cache::WriterLease>>,
    read_marker: Option<crate::root_cache::ReadMarker>,
    conn: Mutex<TrackedConnection>,
    memory: RwLock<HashMap<JobKey, MemoryAggregate>>,
}

#[derive(Debug)]
pub struct ReadonlyInspectCache {
    inner: InspectCache,
}

impl InspectCache {
    /// Estimate only the in-process aggregate map. SQLite-owned allocations are
    /// measured once by the process-wide SQLite allocator counters.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let memory = match self.memory.try_read() {
            Ok(memory) => memory,
            Err(_) => return crate::memory::MemoryEstimate::busy(),
        };
        if memory.is_empty() {
            return crate::memory::MemoryEstimate::partial(0)
                .count("memory_aggregates", 0)
                .count("open_generation_handles", 1);
        }
        let aggregate_bytes = memory.iter().fold(0u64, |bytes, (key, aggregate)| {
            bytes
                .saturating_add(std::mem::size_of::<JobKey>() as u64)
                .saturating_add(
                    key.scope_hash
                        .as_ref()
                        .map(|hash| crate::memory::usize_to_u64(hash.len()))
                        .unwrap_or(0),
                )
                .saturating_add(std::mem::size_of::<MemoryAggregate>() as u64)
                .saturating_add(crate::memory::estimated_json_bytes(&aggregate.payload))
                .saturating_add(
                    aggregate
                        .contribution_set_hash
                        .as_ref()
                        .map(|hash| crate::memory::usize_to_u64(hash.len()))
                        .unwrap_or(0),
                )
        });
        let metadata_bytes = crate::memory::path_bytes(&self.project_root)
            .saturating_add(crate::memory::usize_to_u64(self.project_key.len()))
            .saturating_add(crate::memory::path_bytes(&self.sqlite_path));
        crate::memory::MemoryEstimate::partial(aggregate_bytes.saturating_add(metadata_bytes))
            .count("memory_aggregates", memory.len())
            .count("open_generation_handles", 1)
    }
}

pub trait InspectCacheRead {
    fn get_aggregated_for_config(
        &self,
        key: &JobKey,
        config: &Config,
    ) -> Result<Option<serde_json::Value>, InspectCacheError>;
    fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, InspectCacheError>;
    fn contribution_freshness(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<(PathBuf, FileFreshness)>, InspectCacheError>;
    fn load_tier2_contributions(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<ContributionRecord>, InspectCacheError>;
    fn contribution_set_hash_for_config(
        &self,
        category: InspectCategory,
        config: &Config,
    ) -> Result<String, InspectCacheError>;
    fn last_full_run(&self, category: InspectCategory) -> Result<Option<i64>, InspectCacheError>;
}

impl InspectCache {
    pub fn open(inspect_dir: PathBuf, project_root: PathBuf) -> Result<Self, InspectCacheError> {
        let project_key = crate::path_identity::project_scope_key(&project_root);
        let project_inspect_dir = project_inspect_dir(inspect_dir.clone(), &project_key);
        let Some(writer_lease) =
            acquire_writer_lease(&project_inspect_dir, &project_key, &project_root)?
        else {
            return match Self::open_readonly(inspect_dir, project_root.clone())? {
                Some(cache) => Ok(cache.into_inner()),
                None => Self::borrow_only_empty(project_inspect_dir, project_root, project_key),
            };
        };
        let inspect_dir = project_inspect_dir;
        if !writer_lease.verify().map_err(InspectCacheError::from)? {
            return Err(InspectCacheError::Io(std::io::Error::other(
                "inspect writer lease epoch changed before opening cache",
            )));
        }
        std::fs::create_dir_all(&inspect_dir)?;
        let (sqlite_path, generation, needs_publish) =
            resolve_or_create_inspect_target(&inspect_dir, &project_key);
        let conn = TrackedConnection::open(&sqlite_path, SqliteStore::InspectScopeCache)?;
        configure_connection(&conn)?;
        if !writer_lease.verify().map_err(InspectCacheError::from)? {
            return Err(InspectCacheError::Io(std::io::Error::other(
                "inspect writer lease epoch changed before schema initialization",
            )));
        }
        initialize_schema(&conn)?;
        if needs_publish {
            if !writer_lease.verify().map_err(InspectCacheError::from)? {
                return Err(InspectCacheError::Io(std::io::Error::other(
                    "inspect writer lease epoch changed before pointer publish",
                )));
            }
            publish_inspect_pointer(
                &inspect_dir,
                &project_key,
                generation.as_deref().unwrap_or_default(),
            )?;
        }
        Ok(Self::from_connection(
            project_root,
            project_key,
            sqlite_path,
            Some(writer_lease),
            None,
            conn,
        ))
    }

    pub fn open_readonly(
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Option<ReadonlyInspectCache>, InspectCacheError> {
        let project_key = crate::path_identity::project_scope_key(&project_root);
        let inspect_dir = project_inspect_dir(inspect_dir, &project_key);
        let Some((sqlite_path, generation)) = resolve_inspect_target(&inspect_dir, &project_key)
        else {
            return Ok(None);
        };
        let conn = open_readonly_connection(&sqlite_path)?;
        let marker_label = generation.as_deref().unwrap_or("legacy");
        let read_marker = crate::root_cache::ReadMarker::create(&inspect_dir, marker_label)?;
        Ok(Some(ReadonlyInspectCache::from_inner(
            Self::from_connection(
                project_root,
                project_key,
                sqlite_path,
                None,
                Some(read_marker),
                conn,
            ),
        )))
    }

    fn borrow_only_empty(
        inspect_dir: PathBuf,
        project_root: PathBuf,
        project_key: String,
    ) -> Result<Self, InspectCacheError> {
        let conn = TrackedConnection::open_in_memory(SqliteStore::InspectScopeCache)?;
        initialize_schema(&conn)?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Self::from_connection(
            project_root,
            project_key.clone(),
            inspect_dir.join(format!("{project_key}.borrow-only")),
            None,
            None,
            conn,
        ))
    }

    fn from_connection(
        project_root: PathBuf,
        project_key: String,
        sqlite_path: PathBuf,
        writer_lease: Option<Arc<crate::root_cache::WriterLease>>,
        read_marker: Option<crate::root_cache::ReadMarker>,
        conn: TrackedConnection,
    ) -> Self {
        Self {
            project_root,
            project_key,
            sqlite_path,
            writer_lease,
            read_marker,
            conn: Mutex::new(conn),
            memory: RwLock::new(HashMap::new()),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn project_key(&self) -> &str {
        &self.project_key
    }

    pub fn sqlite_path(&self) -> &Path {
        &self.sqlite_path
    }

    pub fn writer_epoch_for_test(&self) -> Option<&str> {
        self.writer_lease.as_ref().map(|lease| lease.epoch())
    }

    fn verify_writer_lease(&self) -> Result<(), InspectCacheError> {
        let Some(lease) = self.writer_lease.as_ref() else {
            return Err(InspectCacheError::Io(std::io::Error::other(
                "inspect cache opened read-only; write API is unavailable",
            )));
        };
        if lease.verify()? {
            Ok(())
        } else {
            Err(InspectCacheError::Io(std::io::Error::other(format!(
                "inspect writer lease for key {} lost epoch {}; aborting write",
                lease.key(),
                lease.epoch()
            ))))
        }
    }

    fn refresh_read_marker(&self) -> Result<(), InspectCacheError> {
        if let Some(marker) = self.read_marker.as_ref() {
            marker.touch_if_due()?;
        }
        Ok(())
    }

    pub fn store_aggregated(
        &self,
        key: JobKey,
        payload: serde_json::Value,
    ) -> Result<(), InspectCacheError> {
        self.verify_writer_lease()?;
        self.store_memory_aggregate(key, payload, None)
    }

    fn store_memory_aggregate(
        &self,
        key: JobKey,
        payload: serde_json::Value,
        contribution_set_hash: Option<String>,
    ) -> Result<(), InspectCacheError> {
        self.memory
            .write()
            .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
            .insert(
                key,
                MemoryAggregate {
                    payload,
                    generated_at: unix_seconds_now(),
                    contribution_set_hash,
                },
            );
        Ok(())
    }

    pub fn get_aggregated(
        &self,
        key: &JobKey,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.get_aggregated_with_config(key, None)
    }

    pub fn get_aggregated_for_config(
        &self,
        key: &JobKey,
        config: &Config,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.get_aggregated_with_config(key, Some(config))
    }

    fn get_aggregated_with_config(
        &self,
        key: &JobKey,
        config: Option<&Config>,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.refresh_read_marker()?;
        if !key.category.is_tier2() {
            return Ok(self
                .memory
                .read()
                .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
                .get(key)
                .map(|entry| entry.payload.clone()));
        }

        let current_hash = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
            contribution_set_hash_with_conn(
                &conn,
                key.category,
                &self.project_key,
                &self.project_root,
                config,
            )?
        };

        let memory_entry = {
            self.memory
                .read()
                .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
                .get(key)
                .cloned()
        };
        if let Some(entry) = memory_entry {
            if entry.contribution_set_hash.as_deref() == Some(current_hash.as_str()) {
                return Ok(Some(entry.payload));
            }
            self.memory
                .write()
                .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
                .remove(key);
        }

        let payload = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
            conn.query_row(
                "SELECT aggregate FROM tier2_aggregates \
                 WHERE category = ?1 AND project_key = ?2 AND contribution_set_hash = ?3",
                params![key.category.as_str(), self.project_key, current_hash],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        };

        match payload {
            Some(bytes) => {
                let value = serde_json::from_slice::<serde_json::Value>(&bytes)?;
                self.store_memory_aggregate(key.clone(), value.clone(), Some(current_hash))?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub fn store_tier2_result(
        &self,
        key: JobKey,
        scanned_files: &[PathBuf],
        contributions: &[FileContribution],
        aggregate: serde_json::Value,
    ) -> Result<(), InspectCacheError> {
        self.store_tier2_result_with_config(key, scanned_files, contributions, aggregate, None)
    }

    pub fn store_tier2_result_for_config(
        &self,
        key: JobKey,
        scanned_files: &[PathBuf],
        contributions: &[FileContribution],
        aggregate: serde_json::Value,
        config: &Config,
    ) -> Result<(), InspectCacheError> {
        self.store_tier2_result_with_config(
            key,
            scanned_files,
            contributions,
            aggregate,
            Some(config),
        )
    }

    fn store_tier2_result_with_config(
        &self,
        key: JobKey,
        scanned_files: &[PathBuf],
        contributions: &[FileContribution],
        aggregate: serde_json::Value,
        config: Option<&Config>,
    ) -> Result<(), InspectCacheError> {
        if !key.category.is_tier2() {
            self.store_aggregated(key, aggregate)?;
            return Ok(());
        }

        self.verify_writer_lease()?;
        let now = unix_seconds_now();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let tx = conn.transaction()?;

        let scanned_relative = scanned_files
            .iter()
            .map(|path| relative_string(&self.project_root, path))
            .collect::<BTreeSet<_>>();
        let existing = existing_contribution_paths(&tx, key.category, &self.project_key)?;
        for file_path in existing {
            if !scanned_relative.contains(&file_path) {
                tx.execute(
                    "DELETE FROM tier2_contributions WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
                    params![key.category.as_str(), self.project_key, file_path],
                )?;
            }
        }

        for contribution in contributions {
            let file_path = relative_string(&self.project_root, &contribution.file_path);
            let blob = serde_json::to_vec(&contribution_with_type_ref_names(
                contribution.contribution.clone(),
                &contribution.type_ref_names,
            ))?;
            tx.execute(
                "INSERT INTO tier2_contributions \
                 (category, project_key, file_path, file_mtime_ns, file_size, file_hash, contribution, generated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(category, project_key, file_path) DO UPDATE SET \
                 file_mtime_ns = excluded.file_mtime_ns, \
                 file_size = excluded.file_size, \
                 file_hash = excluded.file_hash, \
                 contribution = excluded.contribution, \
                 generated_at = excluded.generated_at",
                params![
                    contribution.category.as_str(),
                    self.project_key,
                    file_path,
                    system_time_to_ns(contribution.freshness.mtime),
                    contribution.freshness.size as i64,
                    hash_to_hex(contribution.freshness.content_hash),
                    blob,
                    now,
                ],
            )?;
        }

        let contribution_set_hash = contribution_set_hash_with_conn(
            &tx,
            key.category,
            &self.project_key,
            &self.project_root,
            config,
        )?;
        let aggregate_blob = serde_json::to_vec(&aggregate)?;
        tx.execute(
            "INSERT INTO tier2_aggregates \
             (category, project_key, contribution_set_hash, aggregate, generated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(category, project_key) DO UPDATE SET \
             contribution_set_hash = excluded.contribution_set_hash, \
             aggregate = excluded.aggregate, \
             generated_at = excluded.generated_at",
            params![
                key.category.as_str(),
                self.project_key,
                contribution_set_hash,
                aggregate_blob,
                now,
            ],
        )?;
        tx.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3) \
             ON CONFLICT(category, project_key) DO UPDATE SET last_full_run = excluded.last_full_run",
            params![key.category.as_str(), self.project_key, now],
        )?;
        tx.commit()?;

        self.store_memory_aggregate(key, aggregate, Some(contribution_set_hash))
    }

    pub(crate) fn apply_contribution_updates_for_config(
        &self,
        category: InspectCategory,
        updates: Tier2ContributionUpdates,
        config: &Config,
    ) -> Result<(String, InspectDbTimings), InspectCacheError> {
        self.apply_contribution_updates_with_config(category, updates, Some(config))
    }

    fn apply_contribution_updates_with_config(
        &self,
        category: InspectCategory,
        updates: Tier2ContributionUpdates,
        config: Option<&Config>,
    ) -> Result<(String, InspectDbTimings), InspectCacheError> {
        self.verify_writer_lease()?;
        let now = unix_seconds_now();
        let lock_started = Instant::now();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let mut timings = InspectDbTimings {
            lock_wait: lock_started.elapsed(),
            ..InspectDbTimings::default()
        };
        let transaction_started = Instant::now();
        let tx = conn.transaction()?;

        for relative_file in updates.deletes {
            tx.execute(
                "DELETE FROM tier2_contributions WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
                params![
                    category.as_str(),
                    self.project_key,
                    relative_file.to_string_lossy().to_string()
                ],
            )?;
        }

        for (relative_file, freshness) in updates.metadata_updates {
            tx.execute(
                "UPDATE tier2_contributions \
                 SET file_mtime_ns = ?4, file_size = ?5, file_hash = ?6 \
                 WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
                params![
                    category.as_str(),
                    self.project_key,
                    relative_file.to_string_lossy().to_string(),
                    system_time_to_ns(freshness.mtime),
                    freshness.size as i64,
                    hash_to_hex(freshness.content_hash),
                ],
            )?;
        }

        for contribution in updates.upserts {
            let file_path = relative_string(&self.project_root, &contribution.file_path);
            let blob = serde_json::to_vec(&contribution_with_type_ref_names(
                contribution.contribution.clone(),
                &contribution.type_ref_names,
            ))?;
            tx.execute(
                "INSERT INTO tier2_contributions \
                 (category, project_key, file_path, file_mtime_ns, file_size, file_hash, contribution, generated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(category, project_key, file_path) DO UPDATE SET \
                 file_mtime_ns = excluded.file_mtime_ns, \
                 file_size = excluded.file_size, \
                 file_hash = excluded.file_hash, \
                 contribution = excluded.contribution, \
                 generated_at = excluded.generated_at",
                params![
                    contribution.category.as_str(),
                    self.project_key,
                    file_path,
                    system_time_to_ns(contribution.freshness.mtime),
                    contribution.freshness.size as i64,
                    hash_to_hex(contribution.freshness.content_hash),
                    blob,
                    now,
                ],
            )?;
        }

        let contribution_set_hash = contribution_set_hash_with_conn(
            &tx,
            category,
            &self.project_key,
            &self.project_root,
            config,
        )?;
        tx.commit()?;
        timings.transaction = transaction_started.elapsed();

        self.memory
            .write()
            .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
            .remove(&JobKey::for_project_category(category));

        Ok((contribution_set_hash, timings))
    }

    pub(crate) fn load_aggregate_if_hash_matches(
        &self,
        category: InspectCategory,
        contribution_set_hash: &str,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.refresh_read_marker()?;
        let payload = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
            conn.query_row(
                "SELECT aggregate FROM tier2_aggregates \
                 WHERE category = ?1 AND project_key = ?2 AND contribution_set_hash = ?3",
                params![category.as_str(), self.project_key, contribution_set_hash],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        };

        match payload {
            Some(bytes) => {
                let value = serde_json::from_slice::<serde_json::Value>(&bytes)?;
                self.store_memory_aggregate(
                    JobKey::for_project_category(category),
                    value.clone(),
                    Some(contribution_set_hash.to_string()),
                )?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.refresh_read_marker()?;
        let payload = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
            conn.query_row(
                "SELECT aggregate FROM tier2_aggregates \
                 WHERE category = ?1 AND project_key = ?2 \
                 ORDER BY generated_at DESC LIMIT 1",
                params![category.as_str(), self.project_key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        };

        match payload {
            Some(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .map(Some)
                .map_err(InspectCacheError::from),
            None => Ok(None),
        }
    }

    pub(crate) fn touch_tier2_last_full_run(
        &self,
        category: InspectCategory,
    ) -> Result<i64, InspectCacheError> {
        self.verify_writer_lease()?;
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let tx = conn.transaction()?;
        let previous = tx
            .query_row(
                "SELECT last_full_run FROM tier2_meta WHERE category = ?1 AND project_key = ?2",
                params![category.as_str(), self.project_key],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let now = unix_seconds_now();
        let last_full_run = previous.map_or(now, |previous| now.max(previous.saturating_add(1)));
        tx.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3)              ON CONFLICT(category, project_key) DO UPDATE SET last_full_run = excluded.last_full_run",
            params![category.as_str(), self.project_key, last_full_run],
        )?;
        tx.commit()?;
        Ok(last_full_run)
    }

    pub(crate) fn store_tier2_aggregate(
        &self,
        key: JobKey,
        contribution_set_hash: &str,
        aggregate: serde_json::Value,
    ) -> Result<(), InspectCacheError> {
        if !key.category.is_tier2() {
            self.store_aggregated(key, aggregate)?;
            return Ok(());
        }

        self.verify_writer_lease()?;
        let now = unix_seconds_now();
        let aggregate_blob = serde_json::to_vec(&aggregate)?;
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO tier2_aggregates \
             (category, project_key, contribution_set_hash, aggregate, generated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(category, project_key) DO UPDATE SET \
             contribution_set_hash = excluded.contribution_set_hash, \
             aggregate = excluded.aggregate, \
             generated_at = excluded.generated_at",
            params![
                key.category.as_str(),
                self.project_key,
                contribution_set_hash,
                aggregate_blob,
                now,
            ],
        )?;
        tx.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3) \
             ON CONFLICT(category, project_key) DO UPDATE SET last_full_run = excluded.last_full_run",
            params![key.category.as_str(), self.project_key, now],
        )?;
        tx.commit()?;

        self.store_memory_aggregate(key, aggregate, Some(contribution_set_hash.to_string()))
    }

    pub fn load_tier2_contributions(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<ContributionRecord>, InspectCacheError> {
        self.refresh_read_marker()?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let mut stmt = conn.prepare(
            "SELECT file_path, file_mtime_ns, file_size, file_hash, contribution \
             FROM tier2_contributions \
             WHERE category = ?1 AND project_key = ?2 \
             ORDER BY file_path ASC",
        )?;
        let rows = stmt.query_map(params![category.as_str(), self.project_key], |row| {
            let file_path: String = row.get(0)?;
            let mtime_ns: i64 = row.get(1)?;
            let file_size: i64 = row.get(2)?;
            let file_hash: String = row.get(3)?;
            let contribution: Vec<u8> = row.get(4)?;
            Ok((file_path, mtime_ns, file_size, file_hash, contribution))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (file_path, mtime_ns, file_size, file_hash, contribution) = row?;
            let contribution: serde_json::Value = serde_json::from_slice(&contribution)?;
            let type_ref_names = type_ref_names_from_contribution(&contribution);
            records.push(ContributionRecord {
                category,
                file_path: PathBuf::from(file_path),
                freshness: FileFreshness {
                    mtime: ns_to_system_time(mtime_ns),
                    size: file_size.max(0) as u64,
                    content_hash: hash_from_hex(&file_hash)?,
                },
                contribution,
                type_ref_names,
            });
        }
        Ok(records)
    }

    pub fn delete_tier2_contribution(
        &self,
        category: InspectCategory,
        relative_file: &Path,
    ) -> Result<(), InspectCacheError> {
        self.verify_writer_lease()?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        conn.execute(
            "DELETE FROM tier2_contributions WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
            params![
                category.as_str(),
                self.project_key,
                relative_file.to_string_lossy().to_string()
            ],
        )?;
        Ok(())
    }

    pub fn update_content_fresh_metadata(
        &self,
        category: InspectCategory,
        relative_file: &Path,
        freshness: &FileFreshness,
    ) -> Result<(), InspectCacheError> {
        self.verify_writer_lease()?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        conn.execute(
            "UPDATE tier2_contributions \
             SET file_mtime_ns = ?4, file_size = ?5, file_hash = ?6 \
             WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
            params![
                category.as_str(),
                self.project_key,
                relative_file.to_string_lossy().to_string(),
                system_time_to_ns(freshness.mtime),
                freshness.size as i64,
                hash_to_hex(freshness.content_hash),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn contribution_freshness(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<(PathBuf, FileFreshness)>, InspectCacheError> {
        self.refresh_read_marker()?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let mut stmt = conn.prepare(
            "SELECT file_path, file_mtime_ns, file_size, file_hash \
             FROM tier2_contributions \
             WHERE category = ?1 AND project_key = ?2 \
             ORDER BY file_path ASC",
        )?;
        let rows = stmt.query_map(params![category.as_str(), self.project_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (file_path, mtime_ns, file_size, file_hash) = row?;
            records.push((
                PathBuf::from(file_path),
                FileFreshness {
                    mtime: ns_to_system_time(mtime_ns),
                    size: file_size.max(0) as u64,
                    content_hash: hash_from_hex(&file_hash)?,
                },
            ));
        }
        Ok(records)
    }

    pub fn contribution_set_hash(
        &self,
        category: InspectCategory,
    ) -> Result<String, InspectCacheError> {
        self.contribution_set_hash_with_config(category, None)
    }

    pub fn contribution_set_hash_for_config(
        &self,
        category: InspectCategory,
        config: &Config,
    ) -> Result<String, InspectCacheError> {
        self.contribution_set_hash_with_config(category, Some(config))
    }

    fn contribution_set_hash_with_config(
        &self,
        category: InspectCategory,
        config: Option<&Config>,
    ) -> Result<String, InspectCacheError> {
        self.refresh_read_marker()?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        contribution_set_hash_with_conn(
            &conn,
            category,
            &self.project_key,
            &self.project_root,
            config,
        )
    }

    pub fn last_full_run(
        &self,
        category: InspectCategory,
    ) -> Result<Option<i64>, InspectCacheError> {
        self.refresh_read_marker()?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        conn.query_row(
            "SELECT last_full_run FROM tier2_meta WHERE category = ?1 AND project_key = ?2",
            params![category.as_str(), self.project_key],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(InspectCacheError::from)
    }

    pub fn memory_generated_at(&self, key: &JobKey) -> Result<Option<i64>, InspectCacheError> {
        self.refresh_read_marker()?;
        Ok(self
            .memory
            .read()
            .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
            .get(key)
            .map(|entry| entry.generated_at))
    }
}

impl ReadonlyInspectCache {
    fn from_inner(inner: InspectCache) -> Self {
        Self { inner }
    }

    fn into_inner(self) -> InspectCache {
        self.inner
    }

    pub fn project_root(&self) -> &Path {
        self.inner.project_root()
    }

    pub fn project_key(&self) -> &str {
        self.inner.project_key()
    }

    pub fn sqlite_path(&self) -> &Path {
        self.inner.sqlite_path()
    }

    pub fn get_aggregated_for_config(
        &self,
        key: &JobKey,
        config: &Config,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.inner.get_aggregated_for_config(key, config)
    }

    pub fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.inner.latest_aggregate_any_hash(category)
    }

    pub fn contribution_freshness(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<(PathBuf, FileFreshness)>, InspectCacheError> {
        self.inner.contribution_freshness(category)
    }

    pub fn load_tier2_contributions(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<ContributionRecord>, InspectCacheError> {
        self.inner.load_tier2_contributions(category)
    }

    pub fn contribution_set_hash_for_config(
        &self,
        category: InspectCategory,
        config: &Config,
    ) -> Result<String, InspectCacheError> {
        self.inner
            .contribution_set_hash_for_config(category, config)
    }

    pub fn last_full_run(
        &self,
        category: InspectCategory,
    ) -> Result<Option<i64>, InspectCacheError> {
        self.inner.last_full_run(category)
    }
}

impl InspectCacheRead for InspectCache {
    fn get_aggregated_for_config(
        &self,
        key: &JobKey,
        config: &Config,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        InspectCache::get_aggregated_for_config(self, key, config)
    }
    fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        InspectCache::latest_aggregate_any_hash(self, category)
    }
    fn contribution_freshness(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<(PathBuf, FileFreshness)>, InspectCacheError> {
        InspectCache::contribution_freshness(self, category)
    }
    fn load_tier2_contributions(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<ContributionRecord>, InspectCacheError> {
        InspectCache::load_tier2_contributions(self, category)
    }
    fn contribution_set_hash_for_config(
        &self,
        category: InspectCategory,
        config: &Config,
    ) -> Result<String, InspectCacheError> {
        InspectCache::contribution_set_hash_for_config(self, category, config)
    }
    fn last_full_run(&self, category: InspectCategory) -> Result<Option<i64>, InspectCacheError> {
        InspectCache::last_full_run(self, category)
    }
}

impl<T: InspectCacheRead + ?Sized> InspectCacheRead for Arc<T> {
    fn get_aggregated_for_config(
        &self,
        key: &JobKey,
        config: &Config,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        (**self).get_aggregated_for_config(key, config)
    }
    fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        (**self).latest_aggregate_any_hash(category)
    }
    fn contribution_freshness(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<(PathBuf, FileFreshness)>, InspectCacheError> {
        (**self).contribution_freshness(category)
    }
    fn load_tier2_contributions(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<ContributionRecord>, InspectCacheError> {
        (**self).load_tier2_contributions(category)
    }
    fn contribution_set_hash_for_config(
        &self,
        category: InspectCategory,
        config: &Config,
    ) -> Result<String, InspectCacheError> {
        (**self).contribution_set_hash_for_config(category, config)
    }
    fn last_full_run(&self, category: InspectCategory) -> Result<Option<i64>, InspectCacheError> {
        (**self).last_full_run(category)
    }
}

impl InspectCacheRead for ReadonlyInspectCache {
    fn get_aggregated_for_config(
        &self,
        key: &JobKey,
        config: &Config,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.get_aggregated_for_config(key, config)
    }
    fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        self.latest_aggregate_any_hash(category)
    }
    fn contribution_freshness(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<(PathBuf, FileFreshness)>, InspectCacheError> {
        self.contribution_freshness(category)
    }
    fn load_tier2_contributions(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<ContributionRecord>, InspectCacheError> {
        self.load_tier2_contributions(category)
    }
    fn contribution_set_hash_for_config(
        &self,
        category: InspectCategory,
        config: &Config,
    ) -> Result<String, InspectCacheError> {
        self.contribution_set_hash_for_config(category, config)
    }
    fn last_full_run(&self, category: InspectCategory) -> Result<Option<i64>, InspectCacheError> {
        self.last_full_run(category)
    }
}

fn project_inspect_dir(inspect_dir: PathBuf, project_key: &str) -> PathBuf {
    if inspect_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == project_key)
    {
        inspect_dir
    } else {
        inspect_dir.join(project_key)
    }
}

fn resolve_or_create_inspect_target(
    inspect_dir: &Path,
    project_key: &str,
) -> (PathBuf, Option<String>, bool) {
    if let Some((path, generation)) = resolve_inspect_target(inspect_dir, project_key) {
        return (path, generation, false);
    }
    let generation = inspect_generation_file_name(project_key);
    (inspect_dir.join(&generation), Some(generation), true)
}

fn resolve_inspect_target(
    inspect_dir: &Path,
    project_key: &str,
) -> Option<(PathBuf, Option<String>)> {
    for _ in 0..5 {
        if let Some(generation) = read_inspect_pointer(inspect_dir, project_key) {
            let path = inspect_dir.join(&generation);
            if path.is_file() {
                return Some((path, Some(generation)));
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let legacy = inspect_legacy_sqlite_path(inspect_dir, project_key);
        return legacy.is_file().then_some((legacy, None));
    }
    None
}

fn inspect_generation_file_name(project_key: &str) -> String {
    format!(
        "{project_key}.g{}.{}.sqlite",
        now_nanos(),
        std::process::id()
    )
}

fn inspect_pointer_path(inspect_dir: &Path, project_key: &str) -> PathBuf {
    inspect_dir.join(format!("{project_key}.current"))
}

fn inspect_legacy_sqlite_path(inspect_dir: &Path, project_key: &str) -> PathBuf {
    inspect_dir.join(format!("{project_key}.sqlite"))
}

fn read_inspect_pointer(inspect_dir: &Path, project_key: &str) -> Option<String> {
    let text = std::fs::read_to_string(inspect_pointer_path(inspect_dir, project_key)).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn publish_inspect_pointer(
    inspect_dir: &Path,
    project_key: &str,
    generation: &str,
) -> Result<(), InspectCacheError> {
    let pointer = inspect_pointer_path(inspect_dir, project_key);
    let tmp = inspect_dir.join(format!(
        "{project_key}.current.tmp.{}.{}",
        std::process::id(),
        now_nanos()
    ));
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(generation.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    if let Err(error) = crate::fs_lock::rename_over(&tmp, &pointer) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    crate::fs_lock::sync_parent(&pointer);
    gc_old_inspect_generations(inspect_dir, project_key, generation);
    Ok(())
}

fn gc_old_inspect_generations(inspect_dir: &Path, project_key: &str, current: &str) {
    let Ok(entries) = std::fs::read_dir(inspect_dir) else {
        return;
    };
    let prefix = format!("{project_key}.g");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == current || !name.starts_with(&prefix) || !name.ends_with(".sqlite") {
            continue;
        }
        let path = entry.path();
        let _ = std::fs::remove_file(&path);
        for suffix in INSPECT_SQLITE_SIDECAR_SUFFIXES {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InspectScopeSweepSummary {
    removed: usize,
    bytes: u64,
    skipped_live: usize,
    skipped_marker: usize,
    scanned: usize,
    budget_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct InspectScopeFileStats {
    newest_file: Option<SystemTime>,
    bytes: u64,
}

enum InspectScopeWalk {
    Complete(InspectScopeFileStats),
    BudgetExceeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Default)]
enum InspectScopeCandidateResult {
    #[default]
    Processed,
    Removed {
        bytes: u64,
    },
    SkippedLive,
    SkippedMarker,
    BudgetExceeded,
}

/// Process-wide inspect-scope GC. The caller supplies the scope keys currently
/// bound in this process; the cursor lets the same publication cadence resume
/// after a large or slow first-level directory exceeds its wall-clock budget.
pub(crate) fn sweep_inspect_scope_dirs(
    inspect_root: &Path,
    live_scope_keys: &HashSet<String>,
) -> InspectScopeSweepSummary {
    sweep_inspect_scope_dirs_with_limits(
        inspect_root,
        live_scope_keys,
        INSPECT_SCOPE_SWEEP_BUDGET,
        usize::MAX,
    )
}

fn sweep_inspect_scope_dirs_with_limits(
    inspect_root: &Path,
    live_scope_keys: &HashSet<String>,
    wall_clock_budget: Duration,
    entry_limit: usize,
) -> InspectScopeSweepSummary {
    let started = Instant::now();
    let deadline = started + wall_clock_budget;
    let mut summary = InspectScopeSweepSummary::default();
    let mut entries = match fs::read_dir(inspect_root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                file_type.is_dir().then(|| {
                    (
                        entry.file_name().to_string_lossy().to_string(),
                        entry.path(),
                    )
                })
            })
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let cursor_store = INSPECT_SCOPE_SWEEP_CURSORS.get_or_init(|| Mutex::new(HashMap::new()));
    let last_name = cursor_store.lock().ok().and_then(|cursors| {
        cursors
            .get(inspect_root)
            .and_then(|cursor| cursor.last_name.clone())
    });
    let start_index = last_name
        .as_deref()
        .and_then(|last| entries.iter().position(|(name, _)| name.as_str() > last))
        .unwrap_or(0);
    if start_index > 0 {
        entries.rotate_left(start_index);
    }

    let mut cursor_name = last_name;
    for (processed, (name, path)) in entries.into_iter().enumerate() {
        if processed >= entry_limit || started.elapsed() >= wall_clock_budget {
            summary.budget_exhausted = true;
            break;
        }
        match inspect_scope_candidate(&path, &name, live_scope_keys, deadline) {
            InspectScopeCandidateResult::BudgetExceeded => {
                summary.budget_exhausted = true;
                break;
            }
            InspectScopeCandidateResult::Processed => {}
            InspectScopeCandidateResult::Removed { bytes } => {
                summary.removed += 1;
                summary.bytes = summary.bytes.saturating_add(bytes);
            }
            InspectScopeCandidateResult::SkippedLive => summary.skipped_live += 1,
            InspectScopeCandidateResult::SkippedMarker => summary.skipped_marker += 1,
        }
        summary.scanned += 1;
        cursor_name = Some(name);
    }

    if let Ok(mut cursors) = cursor_store.lock() {
        let cursor = cursors.entry(inspect_root.to_path_buf()).or_default();
        cursor.last_name = summary.budget_exhausted.then_some(cursor_name).flatten();
        if !summary.budget_exhausted {
            cursor.last_name = None;
        }
    }

    if summary.removed > 0 {
        crate::fs_lock::sync_parent(inspect_root);
    }
    crate::slog_info!(
        "inspect scope cache sweep root={} removed={} bytes={} skipped_live={} skipped_marker={} scanned={} budget_exhausted={}",
        inspect_root.display(),
        summary.removed,
        summary.bytes,
        summary.skipped_live,
        summary.skipped_marker,
        summary.scanned,
        summary.budget_exhausted
    );
    summary
}

fn inspect_scope_candidate(
    scope_dir: &Path,
    scope_name: &str,
    live_scope_keys: &HashSet<String>,
    deadline: Instant,
) -> InspectScopeCandidateResult {
    if live_scope_keys.contains(scope_name) {
        return InspectScopeCandidateResult::SkippedLive;
    }

    let stats = match inspect_scope_file_stats(scope_dir, deadline) {
        InspectScopeWalk::Complete(stats) => stats,
        InspectScopeWalk::BudgetExceeded => return InspectScopeCandidateResult::BudgetExceeded,
        InspectScopeWalk::Failed => return InspectScopeCandidateResult::Processed,
    };
    let Some(newest_file) = stats.newest_file else {
        return InspectScopeCandidateResult::Processed;
    };
    let now = SystemTime::now();
    if now.duration_since(newest_file).unwrap_or(Duration::ZERO) < INSPECT_SCOPE_MIN_AGE {
        return InspectScopeCandidateResult::Processed;
    }

    if crate::root_cache::sweep_all_read_markers(scope_dir).protected {
        return InspectScopeCandidateResult::SkippedMarker;
    }

    match fs::remove_dir_all(scope_dir) {
        Ok(()) => InspectScopeCandidateResult::Removed { bytes: stats.bytes },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !scope_dir.exists() => {
            InspectScopeCandidateResult::Removed { bytes: stats.bytes }
        }
        // A pinned directory (notably ERROR_SHARING_VIOLATION on Windows) is
        // left for the next cursor pass; all deletion errors are best effort.
        Err(_) => InspectScopeCandidateResult::Processed,
    }
}

fn inspect_scope_file_stats(scope_dir: &Path, deadline: Instant) -> InspectScopeWalk {
    if Instant::now() >= deadline {
        return InspectScopeWalk::BudgetExceeded;
    }
    let entries = match fs::read_dir(scope_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return InspectScopeWalk::Complete(InspectScopeFileStats::default())
        }
        Err(_) => return InspectScopeWalk::Failed,
    };
    let mut stats = InspectScopeFileStats::default();
    for entry in entries {
        if Instant::now() >= deadline {
            return InspectScopeWalk::BudgetExceeded;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return InspectScopeWalk::Failed,
        };
        let name = entry.file_name();
        if name == "readers" {
            // Marker heartbeats describe readers, not cache freshness. Marker
            // liveness is checked separately after the age predicate.
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => return InspectScopeWalk::Failed,
        };
        if file_type.is_dir() {
            match inspect_scope_file_stats(&entry.path(), deadline) {
                InspectScopeWalk::Complete(child) => merge_scope_file_stats(&mut stats, child),
                other => return other,
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => return InspectScopeWalk::Failed,
        };
        stats.bytes = stats.bytes.saturating_add(metadata.len());
        let Some(modified) = metadata.modified().ok() else {
            return InspectScopeWalk::Failed;
        };
        if stats.newest_file.is_none_or(|newest| modified > newest) {
            stats.newest_file = Some(modified);
        }
    }
    InspectScopeWalk::Complete(stats)
}

fn merge_scope_file_stats(stats: &mut InspectScopeFileStats, child: InspectScopeFileStats) {
    stats.bytes = stats.bytes.saturating_add(child.bytes);
    if child.newest_file > stats.newest_file {
        stats.newest_file = child.newest_file;
    }
}

#[cfg(test)]
fn reset_inspect_scope_sweep_cursor_for_test() {
    if let Some(cursors) = INSPECT_SCOPE_SWEEP_CURSORS.get() {
        cursors.lock().unwrap().clear();
    }
}

fn acquire_writer_lease(
    inspect_dir: &Path,
    project_key: &str,
    project_root: &Path,
) -> Result<Option<Arc<crate::root_cache::WriterLease>>, InspectCacheError> {
    crate::root_cache::WriterLease::acquire_shared_with_timeout(
        crate::root_cache::RootCacheDomain::Inspect,
        inspect_dir,
        project_key,
        project_root,
        INSPECT_WRITER_LEASE_TIMEOUT,
    )
    .map_err(|error| match error {
        crate::fs_lock::AcquireError::Timeout => InspectCacheError::WriterLeaseTimeout,
        crate::fs_lock::AcquireError::Io(error) => InspectCacheError::Io(error),
    })
}

fn open_readonly_connection(path: &Path) -> Result<TrackedConnection, InspectCacheError> {
    let uri = sqlite_readonly_uri(path);
    let conn = TrackedConnection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        SqliteStore::InspectScopeCache,
    )?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(reader_busy_timeout())?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    Ok(conn)
}

fn reader_busy_timeout() -> Duration {
    let jitter = (now_nanos() % 500) as u64;
    Duration::from_millis(250 + jitter)
}

fn sqlite_readonly_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let encoded = percent_encode_sqlite_uri_path(&raw);
    if raw.starts_with('/') {
        format!("file://{encoded}?mode=ro")
    } else if raw.as_bytes().get(1) == Some(&b':') {
        format!("file:///{encoded}?mode=ro")
    } else {
        format!("file:{encoded}?mode=ro")
    }
}

fn percent_encode_sqlite_uri_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn configure_connection(conn: &Connection) -> Result<(), InspectCacheError> {
    // WAL selection can require a database lock. Install the busy handler first
    // so a concurrent inspect reader or writer is waited out instead of making
    // the cache opener fail immediately on Windows.
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // Inspect facts are fully recomputable, so NORMAL avoids a full fsync per
    // commit while WAL transactions and writer-lease publication stay atomic.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<(), InspectCacheError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tier2_contributions (
            category        TEXT NOT NULL,
            project_key     TEXT NOT NULL,
            file_path       TEXT NOT NULL,
            file_mtime_ns   INTEGER NOT NULL,
            file_size       INTEGER NOT NULL,
            file_hash       TEXT NOT NULL,
            contribution    BLOB NOT NULL,
            generated_at    INTEGER NOT NULL,
            PRIMARY KEY (category, project_key, file_path)
        );

        CREATE TABLE IF NOT EXISTS tier2_aggregates (
            category        TEXT NOT NULL,
            project_key     TEXT NOT NULL,
            contribution_set_hash TEXT NOT NULL,
            aggregate       BLOB NOT NULL,
            generated_at    INTEGER NOT NULL,
            PRIMARY KEY (category, project_key)
        );

        CREATE TABLE IF NOT EXISTS tier2_meta (
            category        TEXT NOT NULL,
            project_key     TEXT NOT NULL,
            last_full_run   INTEGER NOT NULL,
            PRIMARY KEY (category, project_key)
        );",
    )?;
    Ok(())
}

fn existing_contribution_paths(
    conn: &Connection,
    category: InspectCategory,
    project_key: &str,
) -> Result<Vec<String>, InspectCacheError> {
    let mut stmt = conn.prepare(
        "SELECT file_path FROM tier2_contributions WHERE category = ?1 AND project_key = ?2",
    )?;
    let rows = stmt.query_map(params![category.as_str(), project_key], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(InspectCacheError::from)
}

fn contribution_set_hash_with_conn(
    conn: &Connection,
    category: InspectCategory,
    project_key: &str,
    project_root: &Path,
    config: Option<&Config>,
) -> Result<String, InspectCacheError> {
    contribution_set_hash_with_version(
        conn,
        category,
        project_key,
        project_root,
        config,
        TIER2_CONTRIBUTION_CACHE_VERSION,
    )
}

fn contribution_set_hash_with_version(
    conn: &Connection,
    category: InspectCategory,
    project_key: &str,
    project_root: &Path,
    config: Option<&Config>,
    cache_version: u32,
) -> Result<String, InspectCacheError> {
    let mut stmt = conn.prepare(
        "SELECT file_path, file_hash FROM tier2_contributions \
         WHERE category = ?1 AND project_key = ?2 ORDER BY file_path ASC",
    )?;
    let rows = stmt.query_map(params![category.as_str(), project_key], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tier2-contributions\0");
    hasher.update(&cache_version.to_le_bytes());
    hasher.update(b"\0");
    for row in rows {
        let (file_path, file_hash) = row?;
        hasher.update(file_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(file_hash.as_bytes());
        hasher.update(b"\0");
    }
    update_manifest_fingerprint_hash(&mut hasher, project_root)?;
    if matches!(
        category,
        InspectCategory::DeadCode | InspectCategory::UnusedExports | InspectCategory::Cycles
    ) {
        update_resolver_config_fingerprint_hash(&mut hasher, project_root)?;
    }
    update_inspect_config_fingerprint_hash(&mut hasher, category, config);
    Ok(hasher.finalize().to_hex().to_string())
}

fn update_inspect_config_fingerprint_hash(
    hasher: &mut blake3::Hasher,
    category: InspectCategory,
    config: Option<&Config>,
) {
    if category != InspectCategory::Duplicates {
        return;
    }

    hasher.update(b"inspect.duplicates.expected_mirrors\0");
    let Some(config) = config else {
        return;
    };
    for pair in &config.inspect.duplicates.expected_mirrors {
        hasher.update(pair[0].as_bytes());
        hasher.update(b"\0");
        hasher.update(pair[1].as_bytes());
        hasher.update(b"\0");
    }
}

fn update_resolver_config_fingerprint_hash(
    hasher: &mut blake3::Hasher,
    project_root: &Path,
) -> Result<(), InspectCacheError> {
    let manifest_root =
        fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    hasher.update(b"ts-js-resolver-configs\0");
    for config in collect_resolver_config_dependency_files(project_root) {
        let relative_path = config
            .strip_prefix(&manifest_root)
            .unwrap_or(config.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let content_hash = blake3::hash(&fs::read(&config)?);
        hasher.update(relative_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(content_hash.as_bytes());
        hasher.update(b"\0");
    }
    Ok(())
}

struct ResolverConfigDependency {
    path: PathBuf,
    follow_extends: bool,
}

impl ResolverConfigDependency {
    fn resolver_config(path: PathBuf) -> Self {
        Self {
            path,
            follow_extends: true,
        }
    }

    fn hashed_file(path: PathBuf) -> Self {
        Self {
            path,
            follow_extends: false,
        }
    }
}

fn collect_resolver_config_dependency_files(project_root: &Path) -> BTreeSet<PathBuf> {
    let mut configs = walk_resolver_config_files(project_root);
    let mut pending = configs.iter().cloned().collect::<Vec<_>>();
    let mut queued = configs.clone();
    while let Some(config) = pending.pop() {
        for dependency in resolver_config_extends_targets(&config, project_root) {
            let ResolverConfigDependency {
                path,
                follow_extends,
            } = dependency;
            configs.insert(path.clone());
            if follow_extends && queued.insert(path.clone()) {
                pending.push(path);
            }
        }
    }
    configs
}

fn walk_resolver_config_files(project_root: &Path) -> BTreeSet<PathBuf> {
    // Prevent a disappearing child mount from making ReadDir::drop abort on ENXIO.
    let walker = ignore::WalkBuilder::new(project_root)
        .same_file_system(true)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
            {
                return !matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | "venv"
                        | ".venv"
                        | ".git"
                        | "__pycache__"
                        | ".tox"
                        | "dist"
                        | "build"
                );
            }
            true
        })
        .build();

    walker
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        })
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_resolver_config_file_name)
        })
        .filter_map(canonical_file_path)
        .collect()
}

fn is_resolver_config_file_name(name: &str) -> bool {
    name == "tsconfig.json"
        || name == "jsconfig.json"
        || ((name.starts_with("tsconfig.") || name.starts_with("jsconfig."))
            && name.ends_with(".json"))
}

fn resolver_config_extends_targets(
    config: &Path,
    project_root: &Path,
) -> Vec<ResolverConfigDependency> {
    let Ok(source) = fs::read_to_string(config) else {
        return Vec::new();
    };
    let Ok(value) = parse_resolver_config_json(&source) else {
        return Vec::new();
    };

    let mut specs = Vec::new();
    collect_extends_specs(value.get("extends"), &mut specs);
    specs
        .into_iter()
        .flat_map(|spec| resolve_resolver_config_extends(config, project_root, spec))
        .collect()
}

fn parse_resolver_config_json(source: &str) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(source).or_else(|_| serde_json::from_str(&strip_jsonc(source)))
}

fn collect_extends_specs<'a>(value: Option<&'a serde_json::Value>, specs: &mut Vec<&'a str>) {
    match value {
        Some(serde_json::Value::String(spec)) => specs.push(spec),
        Some(serde_json::Value::Array(values)) => {
            for value in values {
                collect_extends_specs(Some(value), specs);
            }
        }
        _ => {}
    }
}

fn resolve_resolver_config_extends(
    config: &Path,
    project_root: &Path,
    spec: &str,
) -> Vec<ResolverConfigDependency> {
    let config_dir = config.parent().unwrap_or(project_root);
    let spec_path = Path::new(spec);
    if spec_path.is_absolute() || spec.starts_with('.') {
        return resolver_config_extends_target(&config_dir.join(spec_path))
            .map(ResolverConfigDependency::resolver_config)
            .into_iter()
            .collect();
    }

    node_modules_resolver_config_dependencies(config_dir, project_root, spec)
}

fn node_modules_resolver_config_dependencies(
    config_dir: &Path,
    project_root: &Path,
    spec: &str,
) -> Vec<ResolverConfigDependency> {
    let boundary = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let config_dir = fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
    let enforce_project_boundary = config_dir.starts_with(&boundary);
    let is_bare_package = is_bare_package_extends_spec(spec);
    let mut dependencies = Vec::new();
    for ancestor in config_dir.ancestors() {
        let ancestor = fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
        if enforce_project_boundary && !ancestor.starts_with(&boundary) {
            break;
        }
        let package_dir = ancestor.join("node_modules").join(spec);
        let mut ancestor_dependencies = Vec::new();
        if is_bare_package {
            if let Some(mut package_dependencies) =
                package_json_resolver_config_dependencies(&package_dir)
            {
                let has_resolver_config = package_dependencies
                    .iter()
                    .any(|dependency| dependency.follow_extends);
                ancestor_dependencies.append(&mut package_dependencies);
                if has_resolver_config {
                    dependencies.extend(ancestor_dependencies);
                    return dependencies;
                }
            }
        }
        if let Some(target) = resolver_config_extends_target(&package_dir) {
            ancestor_dependencies.push(ResolverConfigDependency::resolver_config(target));
            dependencies.extend(ancestor_dependencies);
            return dependencies;
        }
        dependencies.extend(ancestor_dependencies);
    }
    dependencies
}

fn package_json_resolver_config_dependencies(
    package_dir: &Path,
) -> Option<Vec<ResolverConfigDependency>> {
    let package_json = canonical_file_path(package_dir.join("package.json"))?;
    let package_root = package_json
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| package_dir.to_path_buf());
    let mut dependencies = vec![ResolverConfigDependency::hashed_file(package_json.clone())];

    let Ok(source) = fs::read_to_string(&package_json) else {
        return Some(dependencies);
    };
    let Ok(value) = parse_resolver_config_json(&source) else {
        return Some(dependencies);
    };
    let selected_config = value
        .get("tsconfig")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("tsconfig.json");
    if let Some(target) = resolver_config_extends_target(&package_root.join(selected_config)) {
        dependencies.push(ResolverConfigDependency::resolver_config(target));
    }

    Some(dependencies)
}

fn is_bare_package_extends_spec(spec: &str) -> bool {
    let mut parts = spec.split('/').filter(|part| !part.is_empty());
    let Some(first) = parts.next() else {
        return false;
    };
    if first.starts_with('@') {
        parts.next().is_some() && parts.next().is_none()
    } else {
        parts.next().is_none()
    }
}

fn resolver_config_extends_target(base: &Path) -> Option<PathBuf> {
    resolver_config_extends_candidates(base)
        .into_iter()
        .find_map(canonical_file_path)
}

fn resolver_config_extends_candidates(base: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![base.to_path_buf()];
    if base.extension().is_none() {
        candidates.push(base.with_extension("json"));
        candidates.push(base.join("tsconfig.json"));
    }
    candidates
}

fn canonical_file_path(path: PathBuf) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    Some(fs::canonicalize(&path).unwrap_or(path))
}

fn update_manifest_fingerprint_hash(
    hasher: &mut blake3::Hasher,
    project_root: &Path,
) -> Result<(), InspectCacheError> {
    // Entry-point manifests are normalized snapshots. A bare canonical root is
    // verbatim (`\\?\`) on Windows, so mixing it with those non-verbatim
    // manifests makes `strip_prefix` hash an absolute path instead of its
    // project-relative name.
    let manifest_root = crate::inspect::job::canonicalize_normalized(project_root);
    hasher.update(b"entry-point-manifests\0");
    for manifest in super::entry_points::collect_entry_point_manifests(project_root) {
        let relative_path = manifest
            .strip_prefix(&manifest_root)
            .unwrap_or(manifest.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let content_hash = blake3::hash(&fs::read(&manifest)?);
        hasher.update(relative_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(content_hash.as_bytes());
        hasher.update(b"\0");
    }
    Ok(())
}

fn relative_string(project_root: &Path, path: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(project_root) {
        return relative.to_string_lossy().to_string();
    }

    if let (Ok(canonical_root), Ok(canonical_path)) =
        (fs::canonicalize(project_root), fs::canonicalize(path))
    {
        if let Ok(relative) = canonical_path.strip_prefix(canonical_root) {
            return relative.to_string_lossy().to_string();
        }
    }

    path.to_string_lossy().to_string()
}

fn system_time_to_ns(time: SystemTime) -> i64 {
    let nanos = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_nanos();
    nanos.min(i64::MAX as u128) as i64
}

fn ns_to_system_time(value: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(value.max(0) as u64)
}

fn hash_to_hex(hash: blake3::Hash) -> String {
    hash.to_hex().to_string()
}

fn hash_from_hex(value: &str) -> Result<blake3::Hash, InspectCacheError> {
    if value.len() != 64 {
        return Err(InspectCacheError::InvalidHash(value.to_string()));
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk)
            .map_err(|_| InspectCacheError::InvalidHash(value.to_string()))?;
        bytes[index] = u8::from_str_radix(hex, 16)
            .map_err(|_| InspectCacheError::InvalidHash(value.to_string()))?;
    }
    Ok(blake3::Hash::from_bytes(bytes))
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn collect_freshness(path: &Path) -> FileFreshness {
        crate::cache_freshness::collect(path).unwrap()
    }

    #[test]
    fn sqlite_readonly_uri_percent_encodes_windows_paths() {
        assert_eq!(
            sqlite_readonly_uri(Path::new(r"C:\Users\name with spaces\db#1.sqlite")),
            "file:///C:/Users/name%20with%20spaces/db%231.sqlite?mode=ro"
        );
    }

    #[test]
    fn held_writer_lease_fails_within_the_inspect_deadline() {
        let temp = tempfile::tempdir().expect("create temporary cache root");
        let project_root = temp.path().join("project");
        let inspect_dir = temp.path().join("inspect");
        fs::create_dir_all(&project_root).expect("create project root");
        let project_key = crate::path_identity::project_scope_key(&project_root);
        crate::root_cache::configure_artifact_access(&project_root, "shared", false);
        let project_dir = project_inspect_dir(inspect_dir.clone(), &project_key);
        fs::create_dir_all(&project_dir).expect("create project inspect directory");

        // Hold the actual lease file through an independent handle so the test
        // exercises the contended wait rather than an uncontended fast path.
        let held = crate::fs_lock::try_acquire(
            &crate::root_cache::writer_lease_path(&project_dir),
            Duration::ZERO,
        )
        .expect("hold inspect writer lease");
        let started = Instant::now();
        let error = match InspectCache::open(inspect_dir.clone(), project_root.clone()) {
            Err(error) => error,
            Ok(_) => panic!("contended inspect cache must fail honestly"),
        };
        assert!(matches!(error, InspectCacheError::WriterLeaseTimeout));
        assert!(
            started.elapsed() <= INSPECT_WRITER_LEASE_TIMEOUT + Duration::from_secs(1),
            "writer lease acquisition exceeded its deadline"
        );

        drop(held);
        InspectCache::open(inspect_dir, project_root)
            .expect("cache opens after the competing writer releases");
    }

    #[cfg(windows)]
    fn verbatim_path(path: &Path) -> PathBuf {
        PathBuf::from(format!(r"\\?\{}", path.display()))
    }

    #[cfg(windows)]
    #[test]
    fn manifest_fingerprint_uses_relative_paths_for_verbatim_roots() {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().join("project");
        fs::create_dir_all(&project).expect("create project directory");
        let manifest = project.join("package.json");
        let content = br#"{"name":"fixture"}"#;
        fs::write(&manifest, content).expect("write package manifest");

        let normalized_project = crate::inspect::job::canonicalize_normalized(&project);
        let mut actual = blake3::Hasher::new();
        update_manifest_fingerprint_hash(&mut actual, &verbatim_path(&normalized_project))
            .expect("hash entry-point manifest");

        let mut expected = blake3::Hasher::new();
        expected.update(b"entry-point-manifests\0");
        expected.update(b"package.json\0");
        expected.update(blake3::hash(content).as_bytes());
        expected.update(b"\0");

        assert_eq!(actual.finalize(), expected.finalize());
    }

    #[test]
    fn inspect_cache_writer_uses_normal_synchronous_mode() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("checkout");
        fs::create_dir_all(&project_root).unwrap();
        let cache = InspectCache::open(temp.path().join("inspect"), project_root).unwrap();
        let conn = cache.conn.lock().unwrap();
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1, "SQLite NORMAL mode is numeric value 1");
    }

    #[test]
    fn inspect_cache_publishes_pointer_generation_and_reopens_after_crash_redo() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("checkout");
        fs::create_dir_all(&project_root).unwrap();
        let inspect_dir = temp.path().join("inspect");
        let project_key = crate::path_identity::project_scope_key(&project_root);

        fs::create_dir_all(inspect_dir.join("leftover-nonempty-dir")).unwrap();
        let cache = InspectCache::open(inspect_dir.clone(), project_root.clone()).unwrap();
        assert!(cache.sqlite_path().is_file());
        assert_ne!(
            cache.sqlite_path(),
            inspect_dir
                .join(&project_key)
                .join(format!("{project_key}.sqlite"))
        );
        let pointer = inspect_dir
            .join(&project_key)
            .join(format!("{project_key}.current"));
        let generation = fs::read_to_string(&pointer).unwrap();
        assert_eq!(
            inspect_dir.join(&project_key).join(generation.trim()),
            cache.sqlite_path()
        );
        drop(cache);

        let reopened = InspectCache::open(inspect_dir.clone(), project_root.clone()).unwrap();
        assert_eq!(
            inspect_dir.join(&project_key).join(generation.trim()),
            reopened.sqlite_path()
        );
        let readonly = InspectCache::open_readonly(inspect_dir, project_root)
            .unwrap()
            .expect("pointer-published inspect cache should reopen read-only");
        assert_eq!(readonly.inner.sqlite_path(), reopened.sqlite_path());
    }

    fn write_aged_scope_file(scope_dir: &Path, name: &str) {
        fs::create_dir_all(scope_dir.join("nested")).unwrap();
        let path = scope_dir.join("nested").join(name);
        fs::write(&path, b"old inspect payload").unwrap();
        let old = SystemTime::now()
            .checked_sub(INSPECT_SCOPE_MIN_AGE + Duration::from_secs(60))
            .unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
    }

    #[test]
    fn inspect_scope_sweep_reaps_aged_and_keeps_fresh_scope() {
        reset_inspect_scope_sweep_cursor_for_test();
        let temp = tempfile::tempdir().unwrap();
        let inspect_root = temp.path().join("inspect");
        let aged = inspect_root.join("aged-scope");
        let fresh = inspect_root.join("fresh-scope");
        write_aged_scope_file(&aged, "facts.sqlite");
        fs::create_dir_all(&fresh).unwrap();
        fs::write(fresh.join("facts.sqlite"), b"fresh inspect payload").unwrap();

        let summary = sweep_inspect_scope_dirs(&inspect_root, &HashSet::new());

        assert_eq!(summary.removed, 1);
        assert!(summary.bytes > 0, "reaped cache bytes must be reported");
        assert!(!aged.exists(), "an aged scope directory must be reaped");
        assert!(fresh.is_dir(), "a fresh scope directory must survive");
        // This assertion is intentionally a negative check: if the age
        // predicate is accidentally inverted, the fresh directory must still
        // be kept and this assertion will fail.
    }

    #[test]
    fn inspect_scope_sweep_keeps_aged_live_scope_key() {
        reset_inspect_scope_sweep_cursor_for_test();
        let temp = tempfile::tempdir().unwrap();
        let inspect_root = temp.path().join("inspect");
        let project_root = temp.path().join("checkout");
        fs::create_dir_all(&project_root).unwrap();
        let scope_key = crate::path_identity::project_scope_key(&project_root);
        let live = inspect_root.join(&scope_key);
        write_aged_scope_file(&live, "facts.sqlite");
        crate::root_cache::register_live_scope(temp.path(), &project_root);
        let live_keys = crate::root_cache::live_scope_keys_for_storage(temp.path());

        let summary = sweep_inspect_scope_dirs(&inspect_root, &live_keys);

        crate::root_cache::unregister_live_scope(temp.path(), &project_root);
        assert_eq!(summary.skipped_live, 1);
        assert!(live.is_dir(), "a bound root's scope directory must survive");
        // This assertion is intentionally a negative check: if the live-key
        // exemption is removed, the bound root could be deleted and this
        // assertion will fail.
    }

    #[test]
    fn inspect_scope_sweep_keeps_live_marker_then_reaps_stale_marker_scope() {
        reset_inspect_scope_sweep_cursor_for_test();
        let temp = tempfile::tempdir().unwrap();
        let inspect_root = temp.path().join("inspect");
        let scope = inspect_root.join("marked-scope");
        write_aged_scope_file(&scope, "facts.sqlite");
        let marker = crate::root_cache::ReadMarker::create(&scope, "generation").unwrap();
        let mut stale_metadata = marker.metadata().clone();
        stale_metadata.pid = 0;
        fs::write(
            marker.path(),
            serde_json::to_vec(&stale_metadata).expect("marker metadata serializes"),
        )
        .unwrap();

        // Recreate a live marker after planting the stale fixture so the first
        // pass exercises the existing marker liveness rules, not scope age.
        let live_marker = crate::root_cache::ReadMarker::create(&scope, "live-generation").unwrap();
        let first = sweep_inspect_scope_dirs(&inspect_root, &HashSet::new());
        assert_eq!(first.skipped_marker, 1);
        assert!(scope.is_dir(), "a live read marker protects an aged scope");
        drop(live_marker);

        let second = sweep_inspect_scope_dirs(&inspect_root, &HashSet::new());
        assert_eq!(second.removed, 1);
        assert!(
            !scope.exists(),
            "the scope is reapable after markers become stale"
        );
    }

    #[test]
    fn inspect_scope_sweep_cursor_resumes_after_tiny_budget() {
        reset_inspect_scope_sweep_cursor_for_test();
        let temp = tempfile::tempdir().unwrap();
        let inspect_root = temp.path().join("inspect");
        for name in ["scope-a", "scope-b", "scope-c"] {
            write_aged_scope_file(&inspect_root.join(name), "facts.sqlite");
        }

        let first = sweep_inspect_scope_dirs_with_limits(
            &inspect_root,
            &HashSet::new(),
            INSPECT_SCOPE_SWEEP_BUDGET,
            1,
        );
        assert!(first.budget_exhausted);
        assert!(!inspect_root.join("scope-a").exists());
        assert!(inspect_root.join("scope-b").exists());
        assert!(inspect_root.join("scope-c").exists());

        let second = sweep_inspect_scope_dirs_with_limits(
            &inspect_root,
            &HashSet::new(),
            INSPECT_SCOPE_SWEEP_BUDGET,
            1,
        );
        assert!(second.budget_exhausted);
        assert!(!inspect_root.join("scope-b").exists());
        assert!(inspect_root.join("scope-c").exists());

        let third = sweep_inspect_scope_dirs_with_limits(
            &inspect_root,
            &HashSet::new(),
            INSPECT_SCOPE_SWEEP_BUDGET,
            1,
        );
        assert!(!third.budget_exhausted);
        assert!(!inspect_root.join("scope-c").exists());
        reset_inspect_scope_sweep_cursor_for_test();
    }

    #[test]
    fn tier1_file_memo_evicts_lru_and_keeps_recent_hits() {
        let temp = tempfile::tempdir().unwrap();
        let memo = Tier1FileMemo::<usize>::default();
        let mut paths = Vec::with_capacity(TIER1_FILE_MEMO_MAX_ENTRIES);

        for index in 0..TIER1_FILE_MEMO_MAX_ENTRIES {
            let path = temp.path().join(format!("file-{index}.txt"));
            fs::write(&path, index.to_string()).unwrap();
            let value =
                memo.get_or_insert_with(&path, |path| (Some(collect_freshness(path)), index));
            assert_eq!(value, index);
            paths.push(path);
        }

        let recent_path = paths[0].clone();
        let recent_value = memo.get_or_insert_with(&recent_path, |_| {
            panic!("recently inserted entry should hit before eviction")
        });
        assert_eq!(recent_value, 0);

        let evicting_path = temp.path().join("new-file.txt");
        fs::write(&evicting_path, "new").unwrap();
        let evicting_value = memo.get_or_insert_with(&evicting_path, |path| {
            (Some(collect_freshness(path)), TIER1_FILE_MEMO_MAX_ENTRIES)
        });
        assert_eq!(evicting_value, TIER1_FILE_MEMO_MAX_ENTRIES);

        let state = memo.state.lock().unwrap();
        assert_eq!(state.entries.len(), TIER1_FILE_MEMO_MAX_ENTRIES);
        assert!(state.entries.contains_key(&recent_path));
        assert!(state.entries.contains_key(&evicting_path));
        assert!(!state.entries.contains_key(&paths[1]));
        drop(state);

        let recent_value = memo.get_or_insert_with(&recent_path, |_| {
            panic!("recently used entry should survive eviction")
        });
        assert_eq!(recent_value, 0);
    }

    #[test]
    fn tier1_file_memo_full_scope_prunes_paths_no_longer_present() {
        let temp = tempfile::tempdir().unwrap();
        let retained_path = temp.path().join("retained.txt");
        let removed_path = temp.path().join("removed.txt");
        fs::write(&retained_path, "retained").unwrap();
        fs::write(&removed_path, "removed").unwrap();
        let memo = Tier1FileMemo::<usize>::default();

        memo.reserve_for_scan(2);
        memo.get_or_insert_with(&retained_path, |path| (Some(collect_freshness(path)), 1));
        memo.get_or_insert_with(&removed_path, |path| (Some(collect_freshness(path)), 2));
        memo.prune_to_scope(temp.path(), std::slice::from_ref(&retained_path));

        let state = memo.state.lock().unwrap();
        assert!(state.entries.contains_key(&retained_path));
        assert!(!state.entries.contains_key(&removed_path));
        assert_eq!(state.capacity, TIER1_FILE_MEMO_MAX_ENTRIES);
        drop(state);

        let rescanned = Cell::new(false);
        let value = memo.get_or_insert_with(&removed_path, |path| {
            rescanned.set(true);
            (Some(collect_freshness(path)), 3)
        });
        assert!(
            rescanned.get(),
            "a path outside the latest full scope must be evicted"
        );
        assert_eq!(value, 3);
    }

    #[test]
    fn tier1_file_memo_repeated_touches_keep_lazy_lru_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let memo = Tier1FileMemo::<usize>::default();
        let mut paths = Vec::with_capacity(TIER1_FILE_MEMO_MAX_ENTRIES);

        for index in 0..TIER1_FILE_MEMO_MAX_ENTRIES {
            let path = temp.path().join(format!("file-{index}.txt"));
            fs::write(&path, index.to_string()).unwrap();
            memo.get_or_insert_with(&path, |path| (Some(collect_freshness(path)), index));
            paths.push(path);
        }

        for _ in 0..(TIER1_FILE_MEMO_MAX_ENTRIES * 3) {
            let value = memo.get_or_insert_with(&paths[0], |_| {
                panic!("hot entry should stay cached while it is repeatedly touched")
            });
            assert_eq!(value, 0);
        }

        let evicting_path = temp.path().join("new-file.txt");
        fs::write(&evicting_path, "new").unwrap();
        memo.get_or_insert_with(&evicting_path, |path| {
            (Some(collect_freshness(path)), TIER1_FILE_MEMO_MAX_ENTRIES)
        });

        let state = memo.state.lock().unwrap();
        assert_eq!(state.entries.len(), TIER1_FILE_MEMO_MAX_ENTRIES);
        assert!(state.entries.contains_key(&paths[0]));
        assert!(state.entries.contains_key(&evicting_path));
        assert!(!state.entries.contains_key(&paths[1]));
        assert!(
            state.lru.len() <= TIER1_FILE_MEMO_MAX_ENTRIES * 2,
            "lazy LRU queue should be compacted instead of growing without bound"
        );
    }

    #[test]
    fn tier1_file_memo_reuses_fresh_entries_and_rescans_stale_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("memo.txt");
        fs::write(&path, "first").unwrap();

        let memo = Tier1FileMemo::<String>::default();
        let scans = Cell::new(0);

        let first = memo.get_or_insert_with(&path, |path| {
            scans.set(scans.get() + 1);
            (Some(collect_freshness(path)), "first scan".to_string())
        });
        assert_eq!(first, "first scan");
        assert_eq!(scans.get(), 1);

        let unchanged =
            memo.get_or_insert_with(&path, |_| panic!("unchanged file should reuse Tier-1 memo"));
        assert_eq!(unchanged, "first scan");
        assert_eq!(scans.get(), 1);

        fs::write(&path, "changed file contents").unwrap();
        let changed = memo.get_or_insert_with(&path, |path| {
            scans.set(scans.get() + 1);
            (Some(collect_freshness(path)), "second scan".to_string())
        });
        assert_eq!(changed, "second scan");
        assert_eq!(scans.get(), 2);

        let fresh_after_rescan = memo.get_or_insert_with(&path, |_| {
            panic!("rescanned file should reuse refreshed Tier-1 memo")
        });
        assert_eq!(fresh_after_rescan, "second scan");
        assert_eq!(scans.get(), 2);
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct RoundTripContributionRecord {
        category: String,
        file_path: PathBuf,
        contribution: serde_json::Value,
        type_ref_names: BTreeSet<String>,
    }

    impl From<&ContributionRecord> for RoundTripContributionRecord {
        fn from(record: &ContributionRecord) -> Self {
            Self {
                category: record.category.as_str().to_string(),
                file_path: record.file_path.clone(),
                contribution: record.contribution.clone(),
                type_ref_names: record.type_ref_names.clone(),
            }
        }
    }

    #[test]
    fn contribution_record_round_trip_preserves_dead_code_liveness_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        let inspect_dir = temp.path().join("inspect");
        let source = project_root.join("src/lib.ts");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "export interface Widget { id: string }\n").unwrap();

        let cache = InspectCache::open(inspect_dir.clone(), project_root.clone()).unwrap();
        let contribution = FileContribution::new(
            InspectCategory::DeadCode,
            source.clone(),
            collect_freshness(&source),
            serde_json::json!({
                "file": "src/lib.ts",
                "exports": [{
                    "symbol": "Widget",
                    "kind": "interface",
                    "line": 1,
                    "is_type_like": true,
                    "is_entry_point": false,
                }],
                "internal_calls": [],
                "liveness_roots": [],
                "dispatched_method_names": ["render"],
                "macro_token_refs": [{
                    "caller_symbol": "render",
                    "line": 1,
                    "name": "Widget",
                    "shape": "struct"
                }],
                "cfg_test_ranges": [{"start_line": 10, "end_line": 20}],
                "type_ref_names": ["Widget"],
            }),
        )
        .with_type_ref_names(["Widget".to_string()]);
        cache
            .store_tier2_result(
                JobKey::for_project_category(InspectCategory::DeadCode),
                std::slice::from_ref(&source),
                &[contribution],
                serde_json::json!({ "count": 0, "items": [] }),
            )
            .unwrap();
        drop(cache);

        let cache = InspectCache::open(inspect_dir, project_root).unwrap();
        let records = cache
            .load_tier2_contributions(InspectCategory::DeadCode)
            .unwrap();
        assert_eq!(records.len(), 1);

        let serialized =
            serde_json::to_vec(&RoundTripContributionRecord::from(&records[0])).unwrap();
        let decoded: RoundTripContributionRecord = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(decoded.category, InspectCategory::DeadCode.as_str());
        assert_eq!(decoded.contribution["dispatched_method_names"][0], "render");
        assert_eq!(decoded.contribution["type_ref_names"][0], "Widget");
        assert_eq!(
            decoded.contribution["macro_token_refs"][0]["shape"],
            "struct"
        );
        assert_eq!(
            decoded.contribution["cfg_test_ranges"][0],
            serde_json::json!({"start_line": 10, "end_line": 20})
        );
        assert!(decoded.type_ref_names.contains("Widget"));
        assert_eq!(
            decoded.contribution["exports"][0]["is_type_like"].as_bool(),
            Some(true)
        );
        assert_eq!(TIER2_CONTRIBUTION_CACHE_VERSION, 34);
    }

    #[test]
    fn complexity_cache_version_rejects_a_legacy_aggregate_hash() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        let source = project_root.join("src/hot.rs");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "fn hot() {}\n").unwrap();
        let cache = InspectCache::open(temp.path().join("inspect"), project_root.clone()).unwrap();
        let contribution = FileContribution::new(
            InspectCategory::Complexity,
            source.clone(),
            collect_freshness(&source),
            serde_json::json!({
                "file": "src/hot.rs",
                "language": "rust",
                "functions": [{ "function": "hot", "line": 1, "complexity": 10 }],
            }),
        );
        let config = Config::default();
        cache
            .store_tier2_result_for_config(
                JobKey::for_project_category(InspectCategory::Complexity),
                std::slice::from_ref(&source),
                &[contribution],
                serde_json::json!({ "count": 1, "items": [] }),
                &config,
            )
            .unwrap();

        let current_hash = cache
            .contribution_set_hash_for_config(InspectCategory::Complexity, &config)
            .unwrap();
        let legacy_hash = contribution_set_hash_with_version(
            &cache.conn.lock().unwrap(),
            InspectCategory::Complexity,
            &cache.project_key,
            &cache.project_root,
            Some(&config),
            32,
        )
        .unwrap();

        assert_ne!(current_hash, legacy_hash);
        assert!(cache
            .load_aggregate_if_hash_matches(InspectCategory::Complexity, &legacy_hash)
            .unwrap()
            .is_none());
        assert!(cache
            .load_aggregate_if_hash_matches(InspectCategory::Complexity, &current_hash)
            .unwrap()
            .is_some());
    }

    #[test]
    fn duplicate_expected_mirrors_participate_in_aggregate_cache_hash() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        fs::create_dir_all(&project_root).unwrap();
        let left = project_root.join("plugin/a.ts");
        let right = project_root.join("pi-plugin/a.ts");
        fs::create_dir_all(left.parent().unwrap()).unwrap();
        fs::create_dir_all(right.parent().unwrap()).unwrap();
        fs::write(&left, "export const value = 1;\n").unwrap();
        fs::write(&right, "export const value = 1;\n").unwrap();

        let cache = InspectCache::open(temp.path().join("inspect"), project_root.clone()).unwrap();
        let contributions = vec![
            FileContribution::new(
                InspectCategory::Duplicates,
                left.clone(),
                collect_freshness(&left),
                serde_json::json!({ "file": "plugin/a.ts", "line_count": 1, "fragments": [] }),
            ),
            FileContribution::new(
                InspectCategory::Duplicates,
                right.clone(),
                collect_freshness(&right),
                serde_json::json!({ "file": "pi-plugin/a.ts", "line_count": 1, "fragments": [] }),
            ),
        ];
        let config = Config::default();
        cache
            .store_tier2_result_for_config(
                JobKey::for_project_category(InspectCategory::Duplicates),
                &[left.clone(), right.clone()],
                &contributions,
                serde_json::json!({ "count": 0, "items": [] }),
                &config,
            )
            .unwrap();

        let without_mirrors = cache
            .contribution_set_hash_for_config(InspectCategory::Duplicates, &config)
            .unwrap();
        let mut mirror_config = Config::default();
        mirror_config.inspect.duplicates.expected_mirrors =
            vec![["plugin/**".to_string(), "pi-plugin/**".to_string()]];
        let with_mirrors = cache
            .contribution_set_hash_for_config(InspectCategory::Duplicates, &mirror_config)
            .unwrap();

        assert_ne!(without_mirrors, with_mirrors);
    }
}

#[cfg(test)]
mod memory_estimate_tests {
    use super::*;

    #[test]
    fn inspect_memory_estimate_is_zero_when_empty_and_nonzero_when_populated() {
        let root = tempfile::tempdir().expect("project root");
        let project_root = std::fs::canonicalize(root.path()).expect("canonical project root");
        let storage = tempfile::tempdir().expect("inspect storage");
        let cache = InspectCache::open(storage.path().to_path_buf(), project_root)
            .expect("open inspect cache");
        assert_eq!(cache.estimated_memory().estimated_bytes, Some(0));

        cache
            .store_aggregated(
                JobKey::for_project_category(InspectCategory::Todos),
                serde_json::json!({"count": 1, "items": [{"text": "resident todo"}]}),
            )
            .expect("store memory aggregate");
        let estimate = cache.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() > 0);
        assert_eq!(estimate.counts["memory_aggregates"], 1);
        assert_eq!(estimate.counts["open_generation_handles"], 1);
    }
}
