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
use std::sync::{Arc, RwLock};

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
///
/// A build's resolver threads read the same segments at once, and one lock
/// for every hit made each hit wait for all the others (12 threads: 4.9 us
/// per hit, against 0.1 us for one thread). So the cache is split into
/// shards by block, each with its own lock, its own share of the budget and
/// its own use order, and a hit takes only its shard's read lock: it stamps
/// the block's last use atomically and leaves the use order alone. The
/// order catches up at eviction, which moves a block it finds used since it
/// was listed instead of evicting it. Eviction is therefore least recently
/// used within a shard; a small cache keeps one shard, so its order is
/// exact.
#[derive(Debug)]
pub struct BlockCache {
    budget: usize,
    shards: Box<[Shard]>,
}

/// Aligned to a cache line (128 bytes covers every target AFT builds for),
/// so one shard's lock traffic never moves its neighbour's line.
#[derive(Debug)]
#[repr(align(128))]
struct Shard {
    budget: usize,
    /// Stamps of use, increasing; every touch and insert takes a new one,
    /// so no two blocks share a stamp.
    clock: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    state: RwLock<State>,
}

/// The most shards a cache splits into.
const MAX_SHARDS: usize = 32;
/// The fewest blocks a shard's budget holds: below that, a shard would
/// evict blocks a read is still using.
const MIN_SHARD_BLOCKS: usize = 64;

#[derive(Debug, Default)]
struct State {
    entries: HashMap<BlockKey, Entry>,
    /// Entries by the stamp they were listed under, the first the next
    /// candidate for eviction. A hit does not move its entry here; eviction
    /// moves an entry whose last use is newer than its listing.
    by_use: BTreeMap<u64, BlockKey>,
    resident: usize,
    evictions: u64,
}

#[derive(Debug)]
struct Entry {
    bytes: Arc<[u8]>,
    /// The stamp of the entry's last use, written by hits under the read
    /// lock.
    used_at: AtomicU64,
    /// The stamp `by_use` lists the entry under.
    listed_at: u64,
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
        let count = (budget_bytes / (MIN_SHARD_BLOCKS * BLOCK_LEN)).clamp(1, MAX_SHARDS);
        // Every shard's budget is at least one block, and together they are
        // exactly the cache's budget.
        let share = budget_bytes / count;
        let shards = (0..count)
            .map(|index| Shard {
                budget: if index == 0 {
                    budget_bytes - share * (count - 1)
                } else {
                    share
                },
                clock: AtomicU64::new(0),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                state: RwLock::default(),
            })
            .collect();
        Self {
            budget: budget_bytes,
            shards,
        }
    }

    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats {
            budget_bytes: self.budget,
            ..CacheStats::default()
        };
        for shard in &*self.shards {
            let state = shard.read();
            stats.resident_bytes += state.resident;
            stats.blocks += state.entries.len();
            stats.hits += shard.hits.load(Ordering::Relaxed);
            stats.misses += shard.misses.load(Ordering::Relaxed);
            stats.evictions += state.evictions;
        }
        stats
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
        let shard = self.shard(key);
        if let Some(bytes) = shard.read().touch(key, &shard.clock) {
            shard.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(bytes);
        }
        shard.misses.fetch_add(1, Ordering::Relaxed);
        let loaded = load()?;
        let mut state = shard.write();
        if let Some(bytes) = state.touch(key, &shard.clock) {
            return Ok(bytes);
        }
        state.insert(key, Arc::clone(&loaded), &shard.clock);
        while state.resident > shard.budget {
            state.evict_oldest();
        }
        Ok(loaded)
    }

    /// Drops every block of one file, so a closed segment's memory is
    /// released at once rather than waiting for eviction.
    pub(crate) fn forget(&self, file: FileToken) {
        for shard in &*self.shards {
            let mut state = shard.write();
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
    }

    /// The bytes the cache holds in memory, measured as [`crate::heap`]
    /// describes: every cached block in its shared allocation, and the two
    /// tables that find a block and order blocks by use. `None` while
    /// another thread holds the cache's lock: the memory census never waits
    /// for it.
    #[must_use]
    pub fn memory_bytes(&self) -> Option<usize> {
        let mut seen = crate::heap::Seen::new();
        let mut total = std::mem::size_of_val(&*self.shards);
        for shard in &*self.shards {
            let state = match shard.state.try_read() {
                Ok(state) => state,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return None,
            };
            total += crate::heap::HeapBytes::heap_bytes(&state.entries, &mut seen)
                + crate::heap::HeapBytes::heap_bytes(&state.by_use, &mut seen);
        }
        Some(total)
    }

    /// The shard holding `key`: a mix of the key's parts, so neighbouring
    /// blocks of one section spread over the shards.
    fn shard(&self, key: BlockKey) -> &Shard {
        let mixed = key.file.0.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ u64::from(key.section).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ key.block.wrapping_mul(0x1656_67B1_9E37_79F9);
        let index = usize::try_from((mixed >> 32) % self.shards.len() as u64)
            .expect("a shard index fits in usize");
        &self.shards[index]
    }
}

impl Shard {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().expect("block cache lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().expect("block cache lock poisoned")
    }
}

impl State {
    /// The cached block for `key`, stamped as used now. Needs only the read
    /// lock.
    fn touch(&self, key: BlockKey, clock: &AtomicU64) -> Option<Arc<[u8]>> {
        let entry = self.entries.get(&key)?;
        let now = clock.fetch_add(1, Ordering::Relaxed) + 1;
        entry.used_at.fetch_max(now, Ordering::Relaxed);
        Some(Arc::clone(&entry.bytes))
    }

    fn insert(&mut self, key: BlockKey, bytes: Arc<[u8]>, clock: &AtomicU64) {
        let now = clock.fetch_add(1, Ordering::Relaxed) + 1;
        self.resident += bytes.len();
        self.by_use.insert(now, key);
        self.entries.insert(
            key,
            Entry {
                bytes,
                used_at: AtomicU64::new(now),
                listed_at: now,
            },
        );
    }

    /// Evicts the least recently used block. A block used since it was
    /// listed is listed again under its last use instead, until the first
    /// listed block is one nothing used since.
    fn evict_oldest(&mut self) {
        while let Some((listed, key)) = self.by_use.pop_first() {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            let used = entry.used_at.load(Ordering::Relaxed);
            if used > listed {
                entry.listed_at = used;
                self.by_use.insert(used, key);
                continue;
            }
            if let Some(entry) = self.entries.remove(&key) {
                self.resident -= entry.bytes.len();
                self.evictions += 1;
            }
            return;
        }
    }

    fn remove(&mut self, key: BlockKey) {
        if let Some(entry) = self.entries.remove(&key) {
            self.by_use.remove(&entry.listed_at);
            self.resident -= entry.bytes.len();
        }
    }
}

crate::heap_bytes_none!(FileToken, BlockKey);
impl crate::heap::HeapBytes for Entry {
    fn heap_bytes(&self, seen: &mut crate::heap::Seen) -> usize {
        // The stamps are plain integers; only the block owns heap memory.
        let Self {
            bytes,
            used_at: _,
            listed_at: _,
        } = self;
        crate::heap::HeapBytes::heap_bytes(bytes, seen)
    }
}

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
        // An empty cache holds only its shards.
        let shards = std::mem::size_of_val(&*cache.shards);
        assert_eq!(cache.memory_bytes(), Some(shards));
        let file = FileToken::next();
        for index in 0..3 {
            cache
                .get_or_load(key(file, index), || Ok::<_, ()>(block(1)))
                .unwrap();
        }
        let measured = cache.memory_bytes().unwrap();
        let blocks = shards + 3 * (crate::heap::COUNTS + BLOCK_LEN);
        assert!(measured > blocks, "{measured} <= {blocks}");
        assert!(
            measured < blocks + 2048,
            "tables of three entries: {measured}"
        );
        let _held = cache.shards[0].write();
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
    fn a_sharded_cache_keeps_its_whole_budget_and_releases_a_file_from_every_shard() {
        let cache = BlockCache::new(4 * MIN_SHARD_BLOCKS * BLOCK_LEN);
        assert_eq!(cache.shards.len(), 4);
        assert_eq!(
            cache.shards.iter().map(|shard| shard.budget).sum::<usize>(),
            cache.budget
        );
        let (closed, open) = (FileToken::next(), FileToken::next());
        let loads = 6 * MIN_SHARD_BLOCKS as u64;
        for index in 0..loads {
            cache
                .get_or_load(key(open, index), || Ok::<_, ()>(block(1)))
                .unwrap();
            let stats = cache.stats();
            assert!(
                stats.resident_bytes <= stats.budget_bytes,
                "{stats:?} after {index}"
            );
        }
        let stats = cache.stats();
        assert_eq!(stats.misses, loads);
        assert!(stats.evictions > 0, "{stats:?}");
        // Each shard is full, so the blocks held are close to the budget.
        assert!(stats.blocks > 3 * MIN_SHARD_BLOCKS, "{stats:?}");
        // The block loaded last is still held.
        cache
            .get_or_load(key(open, loads - 1), || -> Result<Arc<[u8]>, ()> {
                panic!("the newest block is cached")
            })
            .unwrap();

        for index in 0..32 {
            cache
                .get_or_load(key(closed, index), || Ok::<_, ()>(block(2)))
                .unwrap();
        }
        let before = cache.stats().blocks;
        cache.forget(closed);
        let after = cache.stats();
        assert_eq!(after.blocks, before - 32, "{after:?}");
        for shard in &*cache.shards {
            assert!(shard.read().entries.keys().all(|key| key.file != closed));
        }
    }

    /// Hits under read locks while other threads load and evict: every read
    /// gets its own block's bytes, and the budget holds throughout.
    #[test]
    fn concurrent_loads_and_hits_return_their_own_blocks_within_the_budget() {
        let cache = Arc::new(BlockCache::new(2 * MIN_SHARD_BLOCKS * BLOCK_LEN));
        let file = FileToken::next();
        // Three times what the cache holds, so loads keep evicting.
        let blocks = 6 * MIN_SHARD_BLOCKS as u64;
        std::thread::scope(|scope| {
            for thread in 0..8u64 {
                let cache = Arc::clone(&cache);
                scope.spawn(move || {
                    let mut index = thread * 31;
                    for _ in 0..4_000 {
                        index = (index + 97) % blocks;
                        let fill = (index % 251) as u8;
                        let bytes = cache
                            .get_or_load(key(file, index), || Ok::<_, ()>(block(fill)))
                            .unwrap();
                        assert!(bytes.iter().all(|byte| *byte == fill), "block {index}");
                    }
                });
            }
        });
        let stats = cache.stats();
        assert!(stats.resident_bytes <= stats.budget_bytes, "{stats:?}");
        assert_eq!(stats.resident_bytes, stats.blocks * BLOCK_LEN, "{stats:?}");
        assert!(stats.evictions > 0 && stats.hits > 0, "{stats:?}");
        for shard in &*cache.shards {
            let state = shard.read();
            // Every entry is listed exactly once, under its own stamp.
            assert_eq!(state.by_use.len(), state.entries.len());
            for (listed, key) in &state.by_use {
                assert_eq!(state.entries[key].listed_at, *listed);
            }
        }
    }

    #[test]
    fn a_small_cache_keeps_one_shard() {
        assert_eq!(BlockCache::new(BLOCK_LEN).shards.len(), 1);
        assert_eq!(
            BlockCache::new(2 * MIN_SHARD_BLOCKS * BLOCK_LEN - 1)
                .shards
                .len(),
            1
        );
        assert_eq!(BlockCache::new(1 << 40).shards.len(), MAX_SHARDS);
    }

    /// Measures warm hits from many threads at once, as a build's resolver
    /// threads read the same segments: run with `--ignored --nocapture` in a
    /// release build to compare cache designs.
    #[test]
    #[ignore = "measurement; prints nanoseconds per hit"]
    fn measure_concurrent_warm_hits() {
        let cache = Arc::new(BlockCache::new(256 << 20));
        let file = FileToken::next();
        let blocks = 1024;
        for index in 0..blocks {
            cache
                .get_or_load(key(file, index), || Ok::<_, ()>(block(1)))
                .unwrap();
        }
        for threads in [1usize, 4, 12] {
            let hits_per_thread = 400_000u64;
            let started = std::time::Instant::now();
            std::thread::scope(|scope| {
                for thread in 0..threads {
                    let cache = Arc::clone(&cache);
                    scope.spawn(move || {
                        let mut index = thread as u64 * 7919;
                        for _ in 0..hits_per_thread {
                            index = (index + 2_654_435_761) % blocks;
                            let bytes = cache
                                .get_or_load(key(file, index), || -> Result<Arc<[u8]>, ()> {
                                    panic!("every block is cached")
                                })
                                .unwrap();
                            std::hint::black_box(bytes);
                        }
                    });
                }
            });
            let elapsed = started.elapsed();
            println!(
                "threads={threads} wall_ms={} ns_per_hit_per_thread={:.0}",
                elapsed.as_millis(),
                elapsed.as_nanos() as f64 / hits_per_thread as f64
            );
        }
    }

    #[test]
    #[should_panic(expected = "below one block")]
    fn a_budget_below_one_block_is_refused() {
        let _cache = BlockCache::new(BLOCK_LEN - 1);
    }
}
