//! The resident semantic vector arena of one repository family.
//!
//! Every view of the family that is resident in this process reads its
//! semantic vectors from here. A run (one file's embedded chunks under one
//! content key) is decoded from the family blob store once, when a generation
//! is admitted or when a fill stores it, and is then shared by `Arc` with every
//! view whose manifest names that key. The vectors of a key are therefore held
//! and counted once per process however many checkouts use them, and queries
//! read only runs that are already resident: they never decode store rows.
//!
//! The arena also makes embedding single-flight across views. A fill claims
//! each key it is about to embed; another view that needs the same key while
//! the claim is held waits for it instead of calling the model a second time.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::blob_store::v2::{FamilyKey, FamilyStoreReader};
use crate::semantic_index::{SemanticVectors, ViewPayloadProducer};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Resident runs of one family, shared by every view of it in this process.
#[derive(Debug)]
pub struct SemanticArena {
    storage: PathBuf,
    family: String,
    runs: Mutex<HashMap<FamilyKey, Arc<SemanticVectors>>>,
    inflight: Mutex<HashSet<FamilyKey>>,
    released: Condvar,
    decodes: AtomicU64,
    store_reads: AtomicU64,
}

/// Resident arena memory. Every key is counted once, whatever the number of
/// views that reference it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArenaMemory {
    pub runs: usize,
    pub chunks: usize,
    pub bytes: u64,
}

/// The process's arena for `family` in `storage`. Views of the same family
/// get the same arena while any of them holds it.
pub fn arena_for(storage: &Path, family: &str) -> Arc<SemanticArena> {
    type Registry = HashMap<(PathBuf, String), Weak<SemanticArena>>;
    static ARENAS: OnceLock<Mutex<Registry>> = OnceLock::new();
    let mut arenas = lock(ARENAS.get_or_init(|| Mutex::new(HashMap::new())));
    arenas.retain(|_, arena| arena.strong_count() > 0);
    let key = (storage.to_path_buf(), family.to_owned());
    if let Some(arena) = arenas.get(&key).and_then(Weak::upgrade) {
        return arena;
    }
    let arena = Arc::new(SemanticArena {
        storage: key.0.clone(),
        family: key.1.clone(),
        runs: Mutex::new(HashMap::new()),
        inflight: Mutex::new(HashSet::new()),
        released: Condvar::new(),
        decodes: AtomicU64::new(0),
        store_reads: AtomicU64::new(0),
    });
    arenas.insert(key, Arc::downgrade(&arena));
    arena
}

/// Exclusive right to embed one key. Dropping it wakes views waiting on the
/// key, whether or not the run was installed.
#[derive(Debug)]
pub struct ArenaClaim {
    arena: Arc<SemanticArena>,
    key: FamilyKey,
}

impl ArenaClaim {
    pub fn key(&self) -> &FamilyKey {
        &self.key
    }
}

impl Drop for ArenaClaim {
    fn drop(&mut self) {
        lock(&self.arena.inflight).remove(&self.key);
        self.arena.released.notify_all();
    }
}

impl SemanticArena {
    pub fn storage(&self) -> &Path {
        &self.storage
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    /// The resident run for `key`, if one was admitted.
    pub fn get(&self, key: &FamilyKey) -> Option<Arc<SemanticVectors>> {
        lock(&self.runs).get(key).cloned()
    }

    /// Admits `key` from the family store, decoding it once. Returns `None`
    /// when the store has no row for it. A resident run is returned without
    /// touching the store.
    pub fn load(
        &self,
        store: &FamilyStoreReader,
        key: &FamilyKey,
        rel_path: &Path,
        producer: &ViewPayloadProducer<'_>,
    ) -> Result<Option<Arc<SemanticVectors>>, String> {
        if let Some(run) = self.get(key) {
            return Ok(Some(run));
        }
        self.store_reads.fetch_add(1, Ordering::Relaxed);
        let Some(payload) = store.get(key).map_err(|error| error.to_string())? else {
            return Ok(None);
        };
        self.decodes.fetch_add(1, Ordering::Relaxed);
        let run = SemanticVectors::decode_view_payload(&payload, rel_path, producer)?;
        Ok(Some(self.install(*key, run)))
    }

    /// Makes a run that was just stored resident without decoding it again.
    /// When another view installed the key first, its run is kept and
    /// returned, so the key never has two resident copies.
    pub fn install(&self, key: FamilyKey, run: SemanticVectors) -> Arc<SemanticVectors> {
        Arc::clone(lock(&self.runs).entry(key).or_insert_with(|| Arc::new(run)))
    }

    /// Claims `key` for embedding, or returns `None` while another fill holds
    /// it. A claim is only worth taking after `get` and the store missed.
    pub fn claim(self: &Arc<Self>, key: FamilyKey) -> Option<ArenaClaim> {
        lock(&self.inflight).insert(key).then(|| ArenaClaim {
            arena: Arc::clone(self),
            key,
        })
    }

    /// Waits until no fill holds a claim on `key`, or `timeout` passes.
    /// Returns whether the claim was released.
    pub fn wait_released(&self, key: &FamilyKey, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut inflight = lock(&self.inflight);
        while inflight.contains(key) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            inflight = self
                .released
                .wait_timeout(inflight, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Drops runs no view references any more, after a view unbinds or moves
    /// to another generation. Returns how many runs were released.
    pub fn trim(&self) -> usize {
        let mut runs = lock(&self.runs);
        let before = runs.len();
        runs.retain(|_, run| Arc::strong_count(run) > 1);
        before - runs.len()
    }

    pub fn memory(&self) -> ArenaMemory {
        let runs = lock(&self.runs);
        ArenaMemory {
            runs: runs.len(),
            chunks: runs.values().map(|run| run.chunk_count()).sum(),
            bytes: runs
                .values()
                .map(|run| run.resident_bytes())
                .fold(0u64, u64::saturating_add),
        }
    }

    /// Payloads decoded from the store since the arena was created. Queries
    /// never add to it; only admission and fills do.
    pub fn decode_count(&self) -> u64 {
        self.decodes.load(Ordering::Relaxed)
    }

    /// Store rows read since the arena was created.
    pub fn store_reads(&self) -> u64 {
        self.store_reads.load(Ordering::Relaxed)
    }
}
