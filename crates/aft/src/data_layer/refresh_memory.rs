//! Process footprint sampling for call-graph refresh and cold builds.

mod footprint;

use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use footprint::bytes;

const GROWTH_FILE: &str = "callgraph-cold-growth-bytes";
const GROWTH_SAMPLES_FILE: &str = "callgraph-cold-growth-samples";
const COLD_SAMPLE_LIMIT: usize = 5;
const CHECK_INTERVAL: usize = 256;

fn read_cold_growth(family: &Path) -> Option<u64> {
    std::fs::read_to_string(family.join(GROWTH_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn read_cold_samples(family: &Path) -> Option<Vec<u64>> {
    let contents = std::fs::read_to_string(family.join(GROWTH_SAMPLES_FILE)).ok()?;
    let mut samples = Vec::new();
    for value in contents.split_whitespace() {
        if samples.len() == COLD_SAMPLE_LIMIT {
            return None;
        }
        samples.push(value.parse().ok()?);
    }
    (!samples.is_empty()).then_some(samples)
}

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

impl std::fmt::Display for RefreshStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Memory(_) => "call-graph memory ceiling exceeded",
            Self::Cancelled => "cancelled",
        })
    }
}

impl std::error::Error for RefreshStop {}

pub(super) struct RefreshMemory {
    start: Option<u64>,
    ceiling: u64,
    files: AtomicUsize,
    sites: AtomicUsize,
    peak_growth: AtomicU64,
    hit: OnceLock<MemoryLimit>,
    cancellation: Option<crate::executor::JobCancellation>,
    #[cfg(test)]
    sample: Option<Box<dyn Fn(usize, usize) -> Option<u64> + Send + Sync>>,
}

impl RefreshMemory {
    pub(super) fn start(family: &Path) -> Self {
        let cold = read_cold_growth(family).unwrap_or_default();
        let floor = crate::memory::profile::current().refresh_floor_bytes;
        Self::new(bytes(), floor.max(cold))
    }

    fn new(start: Option<u64>, ceiling: u64) -> Self {
        Self {
            start,
            ceiling,
            files: AtomicUsize::new(0),
            sites: AtomicUsize::new(0),
            peak_growth: AtomicU64::new(0),
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

    pub(super) fn peak_growth(&self) -> Option<u64> {
        self.start.map(|_| self.peak_growth.load(Relaxed))
    }

    /// A publication adds fragments serially; check before each one.
    pub(super) fn publication_add(&self) -> Result<(), RefreshStop> {
        self.check()?;
        self.files.fetch_add(1, Relaxed);
        Ok(())
    }

    pub(super) fn check(&self) -> Result<(), RefreshStop> {
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
            self.peak_growth.fetch_max(growth, Relaxed);
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
        let Some(growth) = self.finish() else {
            return;
        };
        let mut samples = read_cold_samples(family)
            .unwrap_or_else(|| read_cold_growth(family).into_iter().collect());
        if samples.len() == COLD_SAMPLE_LIMIT {
            samples.remove(0);
        }
        samples.push(growth);
        let ceiling = samples.iter().copied().max().unwrap_or(growth);
        let history = samples
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        // Keep the maximum in the original one-number file so older versions
        // can still read the ceiling. Both files are best-effort cache writes.
        for (file, contents) in [
            (GROWTH_SAMPLES_FILE, history),
            (GROWTH_FILE, ceiling.to_string()),
        ] {
            let result = crate::durability::atomic_write(
                &family.join(file),
                contents.as_bytes(),
                crate::durability::AtomicWriteDurability::None,
            );
            if let Err(error) = result {
                log::debug!("could not write {file}: {error}");
            }
        }
    }
}

impl Drop for ColdGrowth {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(family: &Path, growth: u64) {
        let (stop, _stopped) = mpsc::channel();
        ColdGrowth {
            stop,
            sampler: Some(thread::spawn(move || growth)),
        }
        .record(family);
    }

    #[test]
    fn refresh_ceiling_uses_the_largest_recent_cold_sample() {
        let family = tempfile::tempdir().unwrap();
        let peak = u64::MAX - 1;
        record(family.path(), peak);
        record(family.path(), peak - 1);

        assert_eq!(
            std::fs::read_to_string(family.path().join(GROWTH_FILE)).unwrap(),
            peak.to_string()
        );
        assert_eq!(RefreshMemory::start(family.path()).ceiling, peak);
    }

    #[test]
    fn cold_growth_history_keeps_only_the_newest_five() {
        let family = tempfile::tempdir().unwrap();
        for growth in [100, 6, 7, 5, 4, 3] {
            record(family.path(), growth);
        }

        assert_eq!(read_cold_samples(family.path()).unwrap(), [6, 7, 5, 4, 3]);
        assert_eq!(read_cold_growth(family.path()), Some(7));
    }

    #[test]
    fn missing_history_starts_from_the_single_cold_growth_number() {
        let family = tempfile::tempdir().unwrap();
        std::fs::write(family.path().join(GROWTH_FILE), "50").unwrap();
        record(family.path(), 40);

        assert_eq!(read_cold_samples(family.path()).unwrap(), [50, 40]);
        assert_eq!(read_cold_growth(family.path()), Some(50));
    }

    #[test]
    fn corrupt_history_starts_from_the_single_cold_growth_number() {
        let family = tempfile::tempdir().unwrap();
        std::fs::write(family.path().join(GROWTH_FILE), "50").unwrap();
        std::fs::write(family.path().join(GROWTH_SAMPLES_FILE), "100\nbad").unwrap();
        record(family.path(), 40);

        assert_eq!(read_cold_samples(family.path()).unwrap(), [50, 40]);
        assert_eq!(read_cold_growth(family.path()), Some(50));
    }
}
