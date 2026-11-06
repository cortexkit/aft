//! One byte-budgeted cache of verified segment blocks, shared by every open
//! segment.
//!
//! Segments are read with positioned reads, never memory maps: a mapped file
//! cannot be deleted on Windows, a file truncated under a mapping crashes the
//! process on Unix, and mapped pages would escape AFT's memory accounting. The
//! cache holds each block only after its checksum verified, so a cached block
//! is always good bytes, and its size is the data layer's resident read memory.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::segment::BLOCK_LEN;

/// A process-wide id for one open segment file. Blocks are keyed by it, so a
/// closed and reopened segment never sees another file's blocks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FileToken(u64);

impl FileToken {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct BlockKey {
    pub(crate) file: FileToken,
    pub(crate) section: u32,
    pub(crate) block: u64,
}

/// Counters for the memory census and health reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CacheStats {
    /// The configured budget in bytes.
    pub budget_bytes: usize,
    /// Bytes of block data held right now.
    pub resident_bytes: usize,
    /// Blocks held right now.
    pub blocks: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

/// Verified segment blocks, evicted least recently used first once their
/// total size passes the budget.
#[derive(Debug)]
pub struct BlockCache {
    budget: usize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<BlockKey, Entry>,
    /// Entries by last use; the first is the next to evict.
    by_use: BTreeMap<u64, BlockKey>,
    clock: u64,
    resident: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

#[derive(Debug)]
struct Entry {
    bytes: Arc<[u8]>,
    used_at: u64,
}

impl BlockCache {
    /// A cache holding at most `budget_bytes` of block data.
    ///
    /// # Panics
    ///
    /// If the budget is smaller than one block: such a cache could not hold
    /// the block a read is waiting for.
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        assert!(
            budget_bytes >= BLOCK_LEN,
            "block cache budget {budget_bytes} is below one block ({BLOCK_LEN} bytes)"
        );
        Self {
            budget: budget_bytes,
            state: Mutex::default(),
        }
    }

    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let state = self.lock();
        CacheStats {
            budget_bytes: self.budget,
            resident_bytes: state.resident,
            blocks: state.entries.len(),
            hits: state.hits,
            misses: state.misses,
            evictions: state.evictions,
        }
    }

    /// The cached block for `key`, or the block `load` returns, which is then
    /// cached. `load` runs without the cache lock held, so a slow read never
    /// blocks other readers; two threads missing the same block may both load
    /// it, and the second keeps the first one's copy.
    pub(crate) fn get_or_load<E>(
        &self,
        key: BlockKey,
        load: impl FnOnce() -> Result<Arc<[u8]>, E>,
    ) -> Result<Arc<[u8]>, E> {
        {
            let mut state = self.lock();
            if let Some(bytes) = state.touch(key) {
                state.hits += 1;
                return Ok(bytes);
            }
            state.misses += 1;
        }
        let loaded = load()?;
        let mut state = self.lock();
        if let Some(bytes) = state.touch(key) {
            return Ok(bytes);
        }
        state.insert(key, Arc::clone(&loaded));
        while state.resident > self.budget {
            state.evict_oldest();
        }
        Ok(loaded)
    }

    /// Drops every block of one file, so a closed segment's memory is
    /// released at once rather than waiting for eviction.
    pub(crate) fn forget(&self, file: FileToken) {
        let mut state = self.lock();
        let doomed: Vec<BlockKey> = state
            .entries
            .keys()
            .filter(|key| key.file == file)
            .copied()
            .collect();
        for key in doomed {
            state.remove(key);
        }
    }

    /// The bytes the cache holds in memory, measured as [`crate::heap`]
    /// describes: every cached block in its shared allocation, and the two
    /// tables that find a block and order blocks by use. `None` while
    /// another thread holds the cache's lock: the memory census never waits
    /// for it.
    #[must_use]
    pub fn memory_bytes(&self) -> Option<usize> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let mut seen = crate::heap::Seen::new();
        Some(
            crate::heap::HeapBytes::heap_bytes(&state.entries, &mut seen)
                + crate::heap::HeapBytes::heap_bytes(&state.by_use, &mut seen),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("block cache lock poisoned")
    }
}

impl State {
    fn touch(&mut self, key: BlockKey) -> Option<Arc<[u8]>> {
        self.clock += 1;
        let now = self.clock;
        let entry = self.entries.get_mut(&key)?;
        self.by_use.remove(&entry.used_at);
        entry.used_at = now;
        self.by_use.insert(now, key);
        Some(Arc::clone(&entry.bytes))
    }

    fn insert(&mut self, key: BlockKey, bytes: Arc<[u8]>) {
        self.clock += 1;
        self.resident += bytes.len();
        self.by_use.insert(self.clock, key);
        self.entries.insert(
            key,
            Entry {
                bytes,
                used_at: self.clock,
            },
        );
    }

    fn evict_oldest(&mut self) {
        if let Some((_, key)) = self.by_use.pop_first()
            && let Some(entry) = self.entries.remove(&key)
        {
            self.resident -= entry.bytes.len();
            self.evictions += 1;
        }
    }

    fn remove(&mut self, key: BlockKey) {
        if let Some(entry) = self.entries.remove(&key) {
            self.by_use.remove(&entry.used_at);
            self.resident -= entry.bytes.len();
        }
    }
}

crate::heap_bytes_none!(FileToken, BlockKey);
crate::heap_bytes_of_fields!(Entry { bytes, used_at });

#[cfg(test)]
mod tests {
    use super::*;

    fn key(file: FileToken, block: u64) -> BlockKey {
        BlockKey {
            file,
            section: 0,
            block,
        }
    }

    fn block(fill: u8) -> Arc<[u8]> {
        Arc::from(vec![fill; BLOCK_LEN])
    }

    #[test]
    fn memory_counts_every_block_and_the_tables_that_find_them() {
        let cache = BlockCache::new(4 * BLOCK_LEN);
        assert_eq!(cache.memory_bytes(), Some(0));
        let file = FileToken::next();
        for index in 0..3 {
            cache
                .get_or_load(key(file, index), || Ok::<_, ()>(block(1)))
                .unwrap();
        }
        let measured = cache.memory_bytes().unwrap();
        let blocks = 3 * (crate::heap::COUNTS + BLOCK_LEN);
        assert!(measured > blocks, "{measured} <= {blocks}");
        assert!(
            measured < blocks + 2048,
            "tables of three entries: {measured}"
        );
        let _held = cache.lock();
        assert_eq!(
            cache.memory_bytes(),
            None,
            "a held lock is never waited for"
        );
    }

    #[test]
    fn hits_skip_the_loader_and_misses_cache_the_result() {
        let cache = BlockCache::new(4 * BLOCK_LEN);
        let file = FileToken::next();
        let first = cache
            .get_or_load(key(file, 0), || Ok::<_, ()>(block(7)))
            .unwrap();
        let again = cache
            .get_or_load(key(file, 0), || -> Result<Arc<[u8]>, ()> {
                panic!("a cached block must not be loaded again")
            })
            .unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.blocks), (1, 1, 1));
        assert_eq!(stats.resident_bytes, BLOCK_LEN);
    }

    #[test]
    fn a_failed_load_caches_nothing() {
        let cache = BlockCache::new(BLOCK_LEN);
        let file = FileToken::next();
        assert_eq!(cache.get_or_load(key(file, 0), || Err("torn")), Err("torn"));
        assert_eq!(cache.stats().blocks, 0);
        assert_eq!(cache.stats().resident_bytes, 0);
    }

    #[test]
    fn the_least_recently_used_block_is_evicted_first() {
        let cache = BlockCache::new(2 * BLOCK_LEN);
        let file = FileToken::next();
        for index in 0..2 {
            cache
                .get_or_load(key(file, index), || Ok::<_, ()>(block(1)))
                .unwrap();
        }
        // Block 0 is used again, so block 1 is now the oldest.
        cache
            .get_or_load(key(file, 0), || -> Result<Arc<[u8]>, ()> {
                panic!("block 0 is cached")
            })
            .unwrap();
        cache
            .get_or_load(key(file, 2), || Ok::<_, ()>(block(2)))
            .unwrap();
        let stats = cache.stats();
        assert_eq!((stats.blocks, stats.evictions), (2, 1));
        assert!(stats.resident_bytes <= stats.budget_bytes);
        let mut reloaded = false;
        cache
            .get_or_load(key(file, 1), || {
                reloaded = true;
                Ok::<_, ()>(block(1))
            })
            .unwrap();
        assert!(
            reloaded,
            "block 1 was the oldest and should have been evicted"
        );
    }

    #[test]
    fn forgetting_a_file_releases_only_its_blocks() {
        let cache = BlockCache::new(8 * BLOCK_LEN);
        let (closed, open) = (FileToken::next(), FileToken::next());
        for index in 0..3 {
            cache
                .get_or_load(key(closed, index), || Ok::<_, ()>(block(1)))
                .unwrap();
        }
        cache
            .get_or_load(key(open, 0), || Ok::<_, ()>(block(2)))
            .unwrap();
        cache.forget(closed);
        let stats = cache.stats();
        assert_eq!(stats.blocks, 1);
        assert_eq!(stats.resident_bytes, BLOCK_LEN);
    }

    #[test]
    #[should_panic(expected = "below one block")]
    fn a_budget_below_one_block_is_refused() {
        let _cache = BlockCache::new(BLOCK_LEN - 1);
    }
}
