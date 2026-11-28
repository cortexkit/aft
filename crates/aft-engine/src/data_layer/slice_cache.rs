//! One byte-budgeted cache of decoded call-graph files, shared by every
//! checkout of the process.
//!
//! A call-graph command decodes the stored files it walks into
//! [`FileSlice`]s. Decoded, a file takes roughly ten times its stored size
//! (on AFT's own repository about 880 MB for a 74 MB stored graph), and one
//! `impact` or `trace_to` walks most of a repository. Kept per checkout with
//! no limit, every checkout that ever answered such a command held its whole
//! decoded graph until it was released, idle worktrees included. Decoding a
//! file again takes well under a millisecond for most files, so a bounded
//! cache that keeps the recently used files of all checkouts costs little
//! time and caps the memory.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::slice::FileSlice;

/// The cache's budget: what decoded call-graph files of all checkouts may
/// hold together. Large enough for a few repositories' worth of the files
/// commands keep coming back to; a whole-graph walk of a large repository
/// evicts as it goes rather than growing past it.
pub(crate) const BUDGET_BYTES: usize = 256 << 20;

/// A process-wide id for one opened set of stored call-graph files, so the
/// files of a closed or replaced set are never served to another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub(crate) struct SourceToken(u64);

impl SourceToken {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

// A token is a plain number; the files it names are counted by the cache.
aft_store::heap_bytes_none!(SourceToken);

/// Counters for the memory census.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct SliceCacheStats {
    pub budget_bytes: usize,
    /// Measured bytes of the decoded files held right now.
    pub resident_bytes: usize,
    pub slices: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

pub(crate) struct SliceCache {
    budget: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<(SourceToken, String), Entry>,
    /// Entries by last use; the first is the next to evict.
    by_use: BTreeMap<u64, (SourceToken, String)>,
    clock: u64,
    resident: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

struct Entry {
    slice: Arc<FileSlice>,
    bytes: usize,
    used_at: u64,
}

/// The process's cache.
pub(crate) fn shared() -> &'static SliceCache {
    static CACHE: OnceLock<SliceCache> = OnceLock::new();
    CACHE.get_or_init(|| SliceCache::new(BUDGET_BYTES))
}

impl SliceCache {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            budget: budget_bytes,
            state: Mutex::default(),
        }
    }

    /// The cached slice of `path` in `source`, counted as a hit or a miss.
    pub(crate) fn get(&self, source: SourceToken, path: &str) -> Option<Arc<FileSlice>> {
        let mut state = self.lock();
        state.clock += 1;
        let now = state.clock;
        let key = (source, path.to_string());
        let Some(entry) = state.entries.get_mut(&key) else {
            state.misses += 1;
            return None;
        };
        let previous = std::mem::replace(&mut entry.used_at, now);
        let slice = Arc::clone(&entry.slice);
        state.by_use.remove(&previous);
        state.by_use.insert(now, key);
        state.hits += 1;
        Some(slice)
    }

    /// Caches `slice`, evicting the least recently used files past the
    /// budget. A slice larger than the whole budget is not kept: the caller
    /// still has it for the command that decoded it. Returns the cached copy
    /// when another thread decoded the same file first.
    pub(crate) fn insert(
        &self,
        source: SourceToken,
        path: &str,
        slice: Arc<FileSlice>,
    ) -> Arc<FileSlice> {
        let bytes = {
            let mut seen = aft_store::heap::Seen::new();
            aft_store::heap::HeapBytes::heap_bytes(&*slice, &mut seen)
                + std::mem::size_of::<FileSlice>()
                + path.len()
        };
        let mut state = self.lock();
        let key = (source, path.to_string());
        if let Some(entry) = state.entries.get(&key) {
            return Arc::clone(&entry.slice);
        }
        if bytes > self.budget {
            return slice;
        }
        state.clock += 1;
        let now = state.clock;
        state.resident += bytes;
        state.by_use.insert(now, key.clone());
        state.entries.insert(
            key,
            Entry {
                slice: Arc::clone(&slice),
                bytes,
                used_at: now,
            },
        );
        while state.resident > self.budget {
            let Some((_, oldest)) = state.by_use.pop_first() else {
                break;
            };
            if let Some(entry) = state.entries.remove(&oldest) {
                state.resident -= entry.bytes;
                state.evictions += 1;
            }
        }
        slice
    }

    /// Drops every file of `source`, so a closed set's memory is released
    /// at once rather than waiting for eviction.
    pub(crate) fn forget(&self, source: SourceToken) {
        let mut state = self.lock();
        let doomed: Vec<(SourceToken, String)> = state
            .entries
            .keys()
            .filter(|(token, _)| *token == source)
            .cloned()
            .collect();
        for key in doomed {
            if let Some(entry) = state.entries.remove(&key) {
                state.by_use.remove(&entry.used_at);
                state.resident -= entry.bytes;
            }
        }
    }

    /// `None` while another thread holds the cache: the census never waits.
    pub(crate) fn stats(&self) -> Option<SliceCacheStats> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(SliceCacheStats {
            budget_bytes: self.budget,
            resident_bytes: state.resident,
            slices: state.entries.len(),
            hits: state.hits,
            misses: state.misses,
            evictions: state.evictions,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(name: &str) -> Arc<FileSlice> {
        Arc::new(FileSlice::empty_for_test(name))
    }

    #[test]
    fn the_least_recently_used_file_is_evicted_past_the_budget() {
        let probe = slice("a.rs");
        let one = {
            let cache = SliceCache::new(usize::MAX);
            let token = SourceToken::next();
            cache.insert(token, "a.rs", Arc::clone(&probe));
            cache.stats().unwrap().resident_bytes
        };
        // Room for two files, not three.
        let cache = SliceCache::new(one * 2 + one / 2);
        let token = SourceToken::next();
        cache.insert(token, "a.rs", slice("a.rs"));
        cache.insert(token, "b.rs", slice("b.rs"));
        // Using a.rs makes b.rs the oldest.
        assert!(cache.get(token, "a.rs").is_some());
        cache.insert(token, "c.rs", slice("c.rs"));
        assert!(cache.get(token, "b.rs").is_none());
        assert!(cache.get(token, "a.rs").is_some());
        assert!(cache.get(token, "c.rs").is_some());
        let stats = cache.stats().unwrap();
        assert_eq!((stats.slices, stats.evictions), (2, 1));
        assert!(stats.resident_bytes <= stats.budget_bytes);
    }

    #[test]
    fn forgetting_a_source_releases_only_its_files() {
        let cache = SliceCache::new(usize::MAX);
        let (first, second) = (SourceToken::next(), SourceToken::next());
        cache.insert(first, "a.rs", slice("a.rs"));
        cache.insert(second, "a.rs", slice("a.rs"));
        cache.forget(first);
        assert!(cache.get(first, "a.rs").is_none());
        assert!(cache.get(second, "a.rs").is_some());
        assert_eq!(cache.stats().unwrap().slices, 1);
    }

    #[test]
    fn a_file_is_cached_once_per_source() {
        let cache = SliceCache::new(usize::MAX);
        let token = SourceToken::next();
        let first = cache.insert(token, "a.rs", slice("a.rs"));
        let second = cache.insert(token, "a.rs", slice("a.rs"));
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.stats().unwrap().slices, 1);
    }
}
