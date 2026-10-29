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
//!
//! Vectors are shared below the run level too. A run entering the arena has
//! each vector swapped for a bit-identical one already resident, so a fill
//! that reuses an unchanged chunk of an edited file, or two keys that embed
//! the same text, hold one allocation instead of one per run.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::blob_store::v2::{DeletionEpoch, FamilyKey, FamilyStoreReader};
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
    runs: Mutex<resident_runs::ResidentRuns>,
    /// Every distinct vector held by a resident run, by content digest.
    vectors: Mutex<VectorPool>,
    inflight: Mutex<HashSet<FamilyKey>>,
    released: Condvar,
    decodes: AtomicU64,
    store_reads: AtomicU64,
    chunk_row_reads: AtomicU64,
    chunk_row_bytes: AtomicU64,
    seeded_chunks: AtomicU64,
    chunk_batch: Mutex<()>,
    #[cfg(test)]
    chunk_shortcut: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    run_seeding: std::sync::atomic::AtomicBool,
}

/// Most chunk keys the chunk index holds. Entries are counted separately from
/// vectors because distinct chunk keys can share one vector allocation.
pub(crate) const MAX_CHUNK_KEYS: usize = 65_536;

/// How one producer names the chunk rows of a stored file run, and the marker
/// row that says those rows were checked when the run was stored.
///
/// A file payload's vectors are not trusted as chunk vectors on their own: a
/// payload imported from a legacy index, or written before chunk rows existed,
/// can hold floats that differ from the chunk rows. A fill writes the marker
/// only after confirming that every vector of the run it stored is bit-equal
/// to the chunk row for that vector's text, as those rows stood at the
/// store's current [`DeletionEpoch`]. The marker key covers the exact payload
/// bytes and that epoch. Chunk rows are immutable while they exist, so the
/// marker holds until some row is deleted; then the store's epoch moves on,
/// no lookup computes the old key again, and the marker is left for GC.
pub(crate) trait ChunkNaming {
    /// The chunk-row key of a chunk whose embedded text has this digest.
    fn chunk_key_for_text_digest(&self, text_digest: &blake3::Hash) -> FamilyKey;
    /// The marker key that vouches for the run whose payload has this digest,
    /// checked against the chunk rows at `epoch`.
    fn verified_run_marker(
        &self,
        payload_digest: &blake3::Hash,
        epoch: &DeletionEpoch,
    ) -> FamilyKey;
}

/// A run just loaded from the store, with what seeding the chunk index from
/// it needs: its payload digest and the chunk-row key of each of its vectors.
#[derive(Debug)]
pub(crate) struct RunSeed {
    payload_digest: blake3::Hash,
    chunk_keys: Vec<FamilyKey>,
    run: Arc<SemanticVectors>,
}

/// One chunk-index entry: the vector, and the store's deletion epoch at which
/// it was known to equal its chunk row. `None` when the store had no epoch.
/// Only entries checked at the store's current epoch can vouch for a run.
#[derive(Debug)]
struct ChunkEntry {
    vector: Weak<[f32]>,
    checked_at: Option<DeletionEpoch>,
}

/// Resident arena memory. Every key is counted once, whatever the number of
/// views that reference it, and every vector allocation once, whatever the
/// number of runs that share it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArenaMemory {
    pub runs: usize,
    pub chunks: usize,
    pub bytes: u64,
    /// Distinct vector allocations, at most `chunks`.
    pub vectors: usize,
    /// Bytes of floats in those allocations; part of `bytes`.
    pub vector_bytes: u64,
}

/// Weak handles to the arena's vectors, keyed by a digest of their bits.
///
/// A dead `Weak` keeps its allocation until it is dropped, so dead handles
/// are pruned whenever `trim` releases runs and whenever the table has
/// doubled since the last prune.
#[derive(Debug, Default)]
struct VectorPool {
    by_digest: HashMap<blake3::Hash, Weak<[f32]>>,
    // Populate only from chunk-row vectors that were read or stored, or from
    // a loaded run whose marker row vouches for it. Any other file payload
    // can contain different legacy floats.
    by_chunk: HashMap<FamilyKey, ChunkEntry>,
    live_after_prune: usize,
    scratch: Vec<u8>,
}

impl VectorPool {
    /// Shares only bit-identical vectors, preserving cached norms and scores.
    fn share(&mut self, run: &mut SemanticVectors) {
        for vector in run.vectors_mut() {
            self.share_vector(vector);
        }
        self.prune_if_grown();
    }

    fn share_vector(&mut self, vector: &mut Arc<[f32]>) {
        self.scratch.clear();
        for value in vector.iter() {
            self.scratch
                .extend_from_slice(&value.to_bits().to_le_bytes());
        }
        let digest = blake3::hash(&self.scratch);
        match self.by_digest.get(&digest).and_then(Weak::upgrade) {
            Some(resident) if same_bits(&resident, vector) => *vector = resident,
            // Keep unequal allocations on a digest collision.
            Some(_) => {}
            None => {
                self.by_digest.insert(digest, Arc::downgrade(vector));
            }
        }
    }

    /// Prunes once the tables have doubled since the last prune.
    fn prune_if_grown(&mut self) {
        if self.by_digest.len() + self.by_chunk.len()
            > self.live_after_prune.max(1024).saturating_mul(2)
        {
            self.prune();
        }
    }

    fn prune(&mut self) {
        self.by_digest.retain(|_, vector| vector.strong_count() > 0);
        self.by_chunk
            .retain(|_, entry| entry.vector.strong_count() > 0);
        // `retain` keeps the table's allocation, which `memory()` reports;
        // return it once the views that filled it are gone.
        self.by_digest.shrink_to_fit();
        self.by_chunk.shrink_to_fit();
        self.live_after_prune = self.by_digest.len() + self.by_chunk.len();
    }
}

fn same_bits(left: &[f32], right: &[f32]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

type Registry = HashMap<(PathBuf, String), Weak<SemanticArena>>;

/// The resident runs of one arena. The map is private to this module and is
/// changed only through [`ResidentRuns::insert`] and
/// [`ResidentRuns::retain_referenced`], which both advance `epoch`, so no
/// change to the runs can leave the cached totals in place.
mod resident_runs {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use super::ArenaMemory;
    use crate::blob_store::v2::FamilyKey;
    use crate::semantic_index::SemanticVectors;

    #[derive(Debug, Default)]
    pub(super) struct ResidentRuns {
        runs: HashMap<FamilyKey, Arc<SemanticVectors>>,
        /// Advanced by every change to `runs`. A resident run never changes
        /// after it is installed, so while this stays the same so do the run
        /// totals.
        epoch: u64,
        /// The run totals of the last walk and the `epoch` they were taken
        /// at. Walking visits every vector of every run, which for a large
        /// family is millions of hash-set inserts; the health rollup asks for
        /// the totals every few seconds while the runs rarely change.
        totals: Option<(u64, ArenaMemory)>,
        /// Full walks of the runs made by [`Self::totals`].
        #[cfg(test)]
        walks: u64,
    }

    impl ResidentRuns {
        pub(super) fn get(&self, key: &FamilyKey) -> Option<Arc<SemanticVectors>> {
            self.runs.get(key).cloned()
        }

        pub(super) fn insert(&mut self, key: FamilyKey, run: Arc<SemanticVectors>) {
            self.runs.insert(key, run);
            self.epoch += 1;
        }

        /// Drops the runs nothing outside the arena holds. Returns how many
        /// were dropped.
        pub(super) fn retain_referenced(&mut self) -> usize {
            let before = self.runs.len();
            self.runs.retain(|_, run| Arc::strong_count(run) > 1);
            let released = before - self.runs.len();
            if released > 0 {
                self.epoch += 1;
            }
            released
        }

        /// Totals of the runs as [`super::SemanticArena::memory`] reports
        /// them, without the chunk index table: every run once, and every
        /// vector allocation once however many runs share it. Walks the runs
        /// only when they changed since the last walk.
        pub(super) fn totals(&mut self) -> ArenaMemory {
            if let Some((epoch, totals)) = self.totals {
                if epoch == self.epoch {
                    return totals;
                }
            }
            #[cfg(test)]
            {
                self.walks += 1;
            }
            let mut seen = HashSet::new();
            let mut memory = ArenaMemory {
                runs: self.runs.len(),
                ..ArenaMemory::default()
            };
            for run in self.runs.values() {
                memory.chunks += run.chunk_count();
                memory.bytes = memory
                    .bytes
                    .saturating_add(run.resident_bytes_without_vectors());
                for vector in run.vectors() {
                    if seen.insert(Arc::as_ptr(vector).cast::<f32>()) {
                        memory.vectors += 1;
                        memory.vector_bytes = memory
                            .vector_bytes
                            .saturating_add(crate::semantic_index::vector_bytes(vector));
                    }
                }
            }
            memory.bytes = memory.bytes.saturating_add(memory.vector_bytes);
            self.totals = Some((self.epoch, memory));
            memory
        }

        #[cfg(test)]
        pub(super) fn walks(&self) -> u64 {
            self.walks
        }
    }
}

fn arenas() -> &'static Mutex<Registry> {
    static ARENAS: OnceLock<Mutex<Registry>> = OnceLock::new();
    ARENAS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every arena alive in this process, one per `(storage, family)`, so the
/// memory census can count each family's runs exactly once.
pub fn live_arenas() -> Vec<Arc<SemanticArena>> {
    lock(arenas()).values().filter_map(Weak::upgrade).collect()
}

/// The process's arena for `family` in `storage`. Views of the same family
/// get the same arena while any of them holds it.
pub fn arena_for(storage: &Path, family: &str) -> Arc<SemanticArena> {
    let mut arenas = lock(arenas());
    arenas.retain(|_, arena| arena.strong_count() > 0);
    let key = (storage.to_path_buf(), family.to_owned());
    if let Some(arena) = arenas.get(&key).and_then(Weak::upgrade) {
        return arena;
    }
    let arena = Arc::new(SemanticArena {
        storage: key.0.clone(),
        family: key.1.clone(),
        runs: Mutex::new(resident_runs::ResidentRuns::default()),
        vectors: Mutex::new(VectorPool::default()),
        inflight: Mutex::new(HashSet::new()),
        released: Condvar::new(),
        decodes: AtomicU64::new(0),
        store_reads: AtomicU64::new(0),
        chunk_row_reads: AtomicU64::new(0),
        chunk_row_bytes: AtomicU64::new(0),
        seeded_chunks: AtomicU64::new(0),
        chunk_batch: Mutex::new(()),
        #[cfg(test)]
        chunk_shortcut: std::sync::atomic::AtomicBool::new(true),
        #[cfg(test)]
        run_seeding: std::sync::atomic::AtomicBool::new(true),
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

/// Where admitted runs are read from: the v2 family store, or a v3 plane
/// store. Verified-run markers are a v2 deletion-epoch optimization; a source
/// without them returns `None` and its runs simply do not seed the chunk index.
pub(crate) trait RunSource {
    fn run_payload(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>, String>;
    fn markers_at_deletion_epoch(
        &self,
        keys: &mut dyn FnMut(&DeletionEpoch) -> Vec<FamilyKey>,
    ) -> Option<(DeletionEpoch, std::collections::HashSet<FamilyKey>)>;
}

impl RunSource for FamilyStoreReader {
    fn run_payload(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>, String> {
        self.get(key).map_err(|error| error.to_string())
    }
    fn markers_at_deletion_epoch(
        &self,
        keys: &mut dyn FnMut(&DeletionEpoch) -> Vec<FamilyKey>,
    ) -> Option<(DeletionEpoch, std::collections::HashSet<FamilyKey>)> {
        self.contains_many_at_deletion_epoch(|epoch| keys(epoch))
            .ok()
            .flatten()
    }
}

impl RunSource for crate::blob_store::v3::PlaneReader {
    fn run_payload(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>, String> {
        self.get(key).map_err(|error| error.to_string())
    }
    fn markers_at_deletion_epoch(
        &self,
        _keys: &mut dyn FnMut(&DeletionEpoch) -> Vec<FamilyKey>,
    ) -> Option<(DeletionEpoch, std::collections::HashSet<FamilyKey>)> {
        None
    }
}

impl SemanticArena {
    #[cfg(test)]
    pub(crate) fn set_chunk_shortcut(&self, enabled: bool) {
        self.chunk_shortcut.store(enabled, Ordering::Relaxed);
    }

    /// Lets a test measure loads that do not seed the chunk index from runs
    /// vouched for by a marker row, which is how loads behaved before.
    #[cfg(test)]
    pub(crate) fn set_run_seeding(&self, enabled: bool) {
        self.run_seeding.store(enabled, Ordering::Relaxed);
    }

    /// Looks up the key derived from the producer identity and exact text.
    /// Weak handles do not keep floats alive. A loaded file run's vectors are
    /// eligible only once its chunk rows were checked: by this process reading
    /// them, or by the fill that stored the run, as its marker row records.
    pub(crate) fn chunk_vector(&self, key: &FamilyKey) -> Option<Arc<[f32]>> {
        #[cfg(test)]
        if !self.chunk_shortcut.load(Ordering::Relaxed) {
            return None;
        }
        self.verified_chunk(key).map(|(vector, _)| vector)
    }

    /// The indexed chunk-row vector for `key`, whether or not a test disabled
    /// the shortcut, with the deletion epoch at which it was known to equal
    /// its chunk row. A fill uses it to check a run before vouching for it.
    pub(crate) fn verified_chunk(
        &self,
        key: &FamilyKey,
    ) -> Option<(Arc<[f32]>, Option<DeletionEpoch>)> {
        let pool = lock(&self.vectors);
        let entry = pool.by_chunk.get(key)?;
        Some((entry.vector.upgrade()?, entry.checked_at))
    }

    /// Records a chunk-row read a fill made because the chunk index missed.
    pub(crate) fn note_chunk_row_read(&self, payload_bytes: usize) {
        self.chunk_row_reads.fetch_add(1, Ordering::Relaxed);
        self.chunk_row_bytes
            .fetch_add(payload_bytes as u64, Ordering::Relaxed);
    }

    /// Serializes bounded chunk-vector batches across all views of the family.
    pub(crate) fn chunk_batch(&self) -> MutexGuard<'_, ()> {
        lock(&self.chunk_batch)
    }

    /// A cold-admitted fill must yield its slot rather than block on this gate.
    pub(crate) fn try_chunk_batch(&self) -> Option<MutexGuard<'_, ()>> {
        match self.chunk_batch.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn pause_resources_are_free(&self) -> bool {
        self.chunk_batch.try_lock().is_ok() && lock(&self.inflight).is_empty()
    }

    pub fn storage(&self) -> &Path {
        &self.storage
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    /// The resident run for `key`, if one was admitted.
    pub fn get(&self, key: &FamilyKey) -> Option<Arc<SemanticVectors>> {
        lock(&self.runs).get(key)
    }

    /// Admits `key` from the family store, decoding it once. Returns `None`
    /// when the store has no row for it. A resident run is returned without
    /// touching the store. A run a marker row vouches for also seeds the
    /// chunk index; see [`Self::seed_verified_runs`].
    pub(crate) fn load(
        &self,
        store: &dyn RunSource,
        key: &FamilyKey,
        rel_path: &Path,
        producer: &ViewPayloadProducer<'_>,
        naming: &dyn ChunkNaming,
    ) -> Result<Option<Arc<SemanticVectors>>, String> {
        let mut seeds = Vec::new();
        let run = self.load_deferring_seed(store, key, rel_path, producer, naming, &mut seeds)?;
        self.seed_verified_runs(store, seeds, naming);
        Ok(run)
    }

    /// [`Self::load`], leaving the chunk-index seeding to the caller: a
    /// nonempty run this call newly installs is added to `seeds`. Admitting a
    /// generation loads many runs and probes all their markers in one batch
    /// afterwards.
    pub(crate) fn load_deferring_seed(
        &self,
        store: &dyn RunSource,
        key: &FamilyKey,
        rel_path: &Path,
        producer: &ViewPayloadProducer<'_>,
        naming: &dyn ChunkNaming,
        seeds: &mut Vec<RunSeed>,
    ) -> Result<Option<Arc<SemanticVectors>>, String> {
        if let Some(run) = self.get(key) {
            return Ok(Some(run));
        }
        self.store_reads.fetch_add(1, Ordering::Relaxed);
        let Some(payload) = store.run_payload(key)? else {
            return Ok(None);
        };
        self.decodes.fetch_add(1, Ordering::Relaxed);
        let (run, text_digests) =
            SemanticVectors::decode_view_payload_with_text_digests(&payload, rel_path, producer)?;
        let (run, ours) = self.install_inner(*key, run);
        // Only the run decoded from `payload` matches the marker computed from
        // it; a run another load installed first is left as it is.
        if ours && run.chunk_count() > 0 && text_digests.len() == run.chunk_count() {
            seeds.push(RunSeed {
                payload_digest: blake3::hash(&payload),
                chunk_keys: text_digests
                    .iter()
                    .map(|digest| naming.chunk_key_for_text_digest(digest))
                    .collect(),
                run: Arc::clone(&run),
            });
        }
        Ok(Some(run))
    }

    /// Makes a stored run resident, sharing equal allocations. File payloads
    /// alone cannot populate the chunk index: legacy floats may not match the
    /// independently immutable chunk rows. Only a run a marker row vouches
    /// for seeds it, through [`Self::seed_verified_runs`].
    pub fn install(&self, key: FamilyKey, run: SemanticVectors) -> Arc<SemanticVectors> {
        self.install_inner(key, run).0
    }

    /// Installs `run` unless one is already resident for `key`. Returns the
    /// resident run and whether it is the one passed in.
    fn install_inner(
        &self,
        key: FamilyKey,
        mut run: SemanticVectors,
    ) -> (Arc<SemanticVectors>, bool) {
        if let Some(resident) = self.get(&key) {
            return (resident, false);
        }
        lock(&self.vectors).share(&mut run);
        let mut runs = lock(&self.runs);
        if let Some(resident) = runs.get(&key) {
            return (resident, false);
        }
        let run = Arc::new(run);
        runs.insert(key, Arc::clone(&run));
        (run, true)
    }

    /// Indexes the chunk vectors of each loaded run whose marker row for the
    /// store's current deletion epoch is present, so a later fill of the same
    /// chunks reads no chunk rows. The epoch and the markers are read in one
    /// read transaction, the markers in one key-ordered batch through the
    /// payload-free membership index. A marker from an earlier epoch is never
    /// probed: some row was deleted, or the counter rebuilt, since it was
    /// written, so one of its chunk rows may since hold another process's
    /// different floats. A store
    /// without a valid epoch, or a failed read, indexes nothing.
    ///
    /// Seeding never evicts an entry: once the index holds
    /// [`MAX_CHUNK_KEYS`] keys, the remaining vectors stay unindexed.
    pub(crate) fn seed_verified_runs(
        &self,
        store: &dyn RunSource,
        seeds: Vec<RunSeed>,
        naming: &dyn ChunkNaming,
    ) {
        #[cfg(test)]
        if !self.run_seeding.load(Ordering::Relaxed) {
            return;
        }
        if seeds.is_empty() {
            return;
        }
        let Some((epoch, present)) = store.markers_at_deletion_epoch(&mut |epoch| {
            seeds
                .iter()
                .map(|seed| naming.verified_run_marker(&seed.payload_digest, epoch))
                .collect()
        }) else {
            return;
        };
        let mut pool = lock(&self.vectors);
        let mut seeded = 0u64;
        let vouched = seeds.iter().filter(|seed| {
            present.contains(&naming.verified_run_marker(&seed.payload_digest, &epoch))
        });
        'runs: for seed in vouched {
            for (vector, key) in seed.run.vectors().zip(&seed.chunk_keys) {
                // Keep a live entry checked at this epoch: it names the same
                // row's floats. Any other entry is replaced by this fresher one.
                match pool.by_chunk.get(key) {
                    Some(resident)
                        if resident.vector.strong_count() > 0
                            && resident.checked_at == Some(epoch) =>
                    {
                        continue;
                    }
                    Some(_) => {}
                    None if pool.by_chunk.len() >= MAX_CHUNK_KEYS => break 'runs,
                    None => {}
                }
                pool.by_chunk.insert(
                    *key,
                    ChunkEntry {
                        vector: Arc::downgrade(vector),
                        checked_at: Some(epoch),
                    },
                );
                seeded += 1;
            }
        }
        pool.prune_if_grown();
        drop(pool);
        self.seeded_chunks.fetch_add(seeded, Ordering::Relaxed);
    }

    /// Records a vector only after its exact chunk row was read or stored.
    /// `checked_at` is the store's deletion epoch read before that read or
    /// store. The entry can vouch for a run only while `checked_at` is still the
    /// store's current epoch: then no row was deleted, so none was replaced,
    /// after the read or store. The caller keeps
    /// this allocation alive until the file run shares it.
    pub(crate) fn remember_chunk(
        &self,
        key: FamilyKey,
        vector: Vec<f32>,
        checked_at: Option<DeletionEpoch>,
    ) -> Arc<[f32]> {
        // Cap index entries separately: distinct chunk keys can share the same
        // vector allocation. Weak references never keep the floats alive.
        let mut vector: Arc<[f32]> = vector.into();
        let mut pool = lock(&self.vectors);
        pool.share_vector(&mut vector);
        if pool.by_chunk.len() >= MAX_CHUNK_KEYS {
            if let Some(old) = pool.by_chunk.keys().next().copied() {
                pool.by_chunk.remove(&old);
            }
        }
        pool.by_chunk.insert(
            key,
            ChunkEntry {
                vector: Arc::downgrade(&vector),
                checked_at,
            },
        );
        pool.prune_if_grown();
        vector
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
        let released = lock(&self.runs).retain_referenced();
        if released > 0 {
            lock(&self.vectors).prune();
        }
        released
    }

    /// Resident bytes of the arena: its runs, each vector allocation once, and
    /// the chunk index table. The run totals are reused while no run was added
    /// or dropped (see `ResidentRuns`), so an unchanged arena is not walked
    /// again.
    pub fn memory(&self) -> ArenaMemory {
        let mut runs = lock(&self.runs);
        let mut memory = runs.totals();
        // The chunk index grows and shrinks without any run changing, so its
        // table size is read on every call; reading it is one lock. The runs
        // stay locked meanwhile, as they always were here.
        memory.bytes = memory.bytes.saturating_add(
            (lock(&self.vectors).by_chunk.capacity()
                * std::mem::size_of::<(FamilyKey, ChunkEntry)>()) as u64,
        );
        drop(runs);
        memory
    }

    #[cfg(test)]
    pub(crate) fn run_memory_walks_for_test(&self) -> u64 {
        lock(&self.runs).walks()
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

    /// Chunk rows fills read because the chunk index missed, and their
    /// payload bytes, since the arena was created.
    pub fn chunk_row_reads(&self) -> (u64, u64) {
        (
            self.chunk_row_reads.load(Ordering::Relaxed),
            self.chunk_row_bytes.load(Ordering::Relaxed),
        )
    }

    /// Chunk-index entries seeded from loaded runs a marker vouched for.
    pub fn seeded_chunks(&self) -> u64 {
        self.seeded_chunks.load(Ordering::Relaxed)
    }

    /// Entries in the chunk index, live or not yet pruned.
    #[cfg(test)]
    pub(crate) fn chunk_index_len(&self) -> usize {
        lock(&self.vectors).by_chunk.len()
    }
}
