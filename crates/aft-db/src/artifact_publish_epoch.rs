/// Serializes artifact supersession with the final disk publication step.
/// Advancing an epoch either happens before a stale worker checks and prevents
/// its publish, or after that worker has fully published the still-current
/// generation. This closes the check-then-publish race of a bare atomic epoch.
#[derive(Clone, Default)]
pub struct ArtifactPublishEpoch {
    current: Arc<parking_lot::Mutex<u64>>,
}

impl ArtifactPublishEpoch {
    pub fn next(&self) -> u64 {
        let mut current = self.current.lock();
        *current = current.wrapping_add(1);
        *current
    }

    pub fn current(&self) -> u64 {
        *self.current.lock()
    }

    pub fn is_current(&self, expected: u64) -> bool {
        *self.current.lock() == expected
    }

    pub fn run_if_current<R>(&self, expected: u64, publish: impl FnOnce() -> R) -> Option<R> {
        let current = self.current.lock();
        if *current != expected {
            return None;
        }
        Some(publish())
    }
}
