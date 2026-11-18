use super::*;

impl DataLayer {
    /// A data layer whose segment reads share a cache of `cache_bytes`, and
    /// whose builds use `threads` threads.
    #[must_use]
    pub fn new(cache_bytes: usize, threads: usize) -> Self {
        Self {
            cache: Arc::new(BlockCache::new(cache_bytes)),
            threads: threads.max(1),
            memory_profile: false,
            policy: crate::data_layer::content::scan_policy(),
            engines: Arc::new(engines::EngineKeeper::new()),
            bound: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn with_memory_profile(threads: usize) -> Self {
        let profile = crate::memory::profile::current();
        let mut layer = Self::new(profile.block_cache_bytes, threads);
        layer.memory_profile = true;
        layer
    }

    pub(crate) fn apply_memory_profile(&self, profile: crate::memory::profile::MemoryProfile) {
        self.cache.set_budget(profile.block_cache_bytes);
    }

    pub(super) fn resolver_threads(&self) -> usize {
        if self.memory_profile {
            crate::memory::profile::current()
                .resolver_threads
                .unwrap_or(self.threads)
        } else {
            self.threads
        }
    }
}
