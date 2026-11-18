//! Process footprint sampling for call-graph refresh and cold builds.

mod footprint;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use footprint::bytes;

const GROWTH_FILE: &str = "callgraph-cold-growth-bytes";
const CHECK_INTERVAL: usize = 256;

#[derive(Clone, Copy, Debug)]
pub(super) struct MemoryLimit {
    pub(super) growth: u64,
    pub(super) ceiling: u64,
    pub(super) sites: usize,
    pub(super) files: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum RefreshStop {
    Memory(MemoryLimit),
    Cancelled,
}

pub(super) struct RefreshMemory {
    start: Option<u64>,
    ceiling: u64,
    files: AtomicUsize,
    sites: AtomicUsize,
    hit: OnceLock<MemoryLimit>,
    cancellation: Option<crate::executor::JobCancellation>,
    #[cfg(test)]
    sample: Option<Box<dyn Fn(usize, usize) -> Option<u64> + Send + Sync>>,
}

impl RefreshMemory {
    pub(super) fn start(family: &Path) -> Self {
        let cold = std::fs::read_to_string(family.join(GROWTH_FILE))
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or_default();
        let floor = crate::memory::profile::current().refresh_floor_bytes;
        Self::new(bytes(), floor.max(cold))
    }

    fn new(start: Option<u64>, ceiling: u64) -> Self {
        Self {
            start,
            ceiling,
            files: AtomicUsize::new(0),
            sites: AtomicUsize::new(0),
            hit: OnceLock::new(),
            cancellation: crate::executor::current_job_cancellation(),
            #[cfg(test)]
            sample: None,
        }
    }

    #[cfg(test)]
    pub(super) fn simulated(
        ceiling: u64,
        sample: impl Fn(usize, usize) -> Option<u64> + Send + Sync + 'static,
    ) -> Self {
        let mut guard = Self::new(Some(0), ceiling);
        guard.sample = Some(Box::new(sample));
        guard
    }

    fn sample(&self) -> Option<u64> {
        #[cfg(test)]
        if let Some(sample) = &self.sample {
            return sample(self.files.load(Relaxed), self.sites.load(Relaxed));
        }
        bytes()
    }

    fn check(&self) -> Result<(), RefreshStop> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(|token| token.cancel_requested_before_commit())
        {
            return Err(RefreshStop::Cancelled);
        }
        if let Some(limit) = self.hit.get() {
            return Err(RefreshStop::Memory(*limit));
        }
        if let Some(growth) = self
            .start
            .zip(self.sample())
            .map(|(start, now)| now.saturating_sub(start))
        {
            if growth > self.ceiling {
                let limit = self.hit.get_or_init(|| MemoryLimit {
                    growth,
                    ceiling: self.ceiling,
                    files: self.files.load(Relaxed),
                    sites: self.sites.load(Relaxed),
                });
                return Err(RefreshStop::Memory(*limit));
            }
        }
        Ok(())
    }

    fn step(&self, counter: &AtomicUsize) -> Result<(), RefreshStop> {
        if let Some(limit) = self.hit.get() {
            return Err(RefreshStop::Memory(*limit));
        }
        // A shared counter keeps the sampling interval independent of the number
        // of resolver workers and catches growth even in a single large file.
        if (counter.fetch_add(1, Relaxed) + 1).is_multiple_of(CHECK_INTERVAL) {
            self.check()?;
        }
        Ok(())
    }
}

/// Unguarded callers use the same resolver paths without sampling or counting.
pub(super) trait RefreshChecks {
    fn boundary(self) -> Result<(), RefreshStop>;
    fn file(self) -> Result<(), RefreshStop>;
    fn site(self) -> Result<(), RefreshStop>;
}

impl RefreshChecks for Option<&RefreshMemory> {
    fn boundary(self) -> Result<(), RefreshStop> {
        self.map_or(Ok(()), RefreshMemory::check)
    }

    fn file(self) -> Result<(), RefreshStop> {
        self.map_or(Ok(()), |guard| guard.step(&guard.files))
    }

    fn site(self) -> Result<(), RefreshStop> {
        self.map_or(Ok(()), |guard| guard.step(&guard.sites))
    }
}

/// Cold passes can peak between their phase boundaries. Sample during the
/// build, without interrupting it, and retain only the largest growth seen.
pub(super) struct ColdGrowth {
    stop: mpsc::Sender<()>,
    sampler: Option<JoinHandle<u64>>,
}

impl ColdGrowth {
    pub(super) fn start() -> Option<Self> {
        let start = bytes()?;
        let (stop, stopped) = mpsc::channel();
        let sampler = thread::Builder::new()
            .name("aft-cold-memory".into())
            .spawn(move || {
                let mut peak = start;
                loop {
                    let result = stopped.recv_timeout(Duration::from_millis(50));
                    peak = peak.max(bytes().unwrap_or_default());
                    if result != Err(mpsc::RecvTimeoutError::Timeout) {
                        return peak - start;
                    }
                }
            })
            .ok()?;
        Some(Self {
            stop,
            sampler: Some(sampler),
        })
    }

    fn finish(&mut self) -> Option<u64> {
        self.sampler.take().and_then(|sampler| {
            let _ = self.stop.send(());
            sampler.join().ok()
        })
    }

    pub(super) fn record(mut self, family: &Path) {
        if let Some(growth) = self.finish() {
            let result = crate::durability::atomic_write(
                &family.join(GROWTH_FILE),
                growth.to_string().as_bytes(),
                crate::durability::AtomicWriteDurability::None,
            );
            if let Err(error) = result {
                log::debug!("could not record cold call-graph build memory growth: {error}");
            }
        }
    }
}

impl Drop for ColdGrowth {
    fn drop(&mut self) {
        self.finish();
    }
}
