use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub enum Source {
    Shell,
    Grants,
    Held,
    Parked,
}
impl Source {
    pub const ALL: [Self; 4] = [Self::Shell, Self::Grants, Self::Held, Self::Parked];
    pub fn index(self) -> usize {
        self as usize
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Grants => "grants",
            Self::Held => "held",
            Self::Parked => "parked",
        }
    }
}
static CHANGED: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);
static REVISIONS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Coalesced hints carry no scope identity or authority. The worker reads the
/// committed state again, so duplicate hints and reconnects are harmless.
pub fn changed(source: Source) {
    REVISIONS[source.index()].fetch_add(1, Ordering::Release);
    CHANGED.notify_waiters();
}
pub fn context_changed() {
    for source in Source::ALL {
        changed(source);
    }
}
pub fn notifier() -> &'static tokio::sync::Notify {
    &CHANGED
}
pub fn revisions() -> [u64; 4] {
    std::array::from_fn(|i| REVISIONS[i].load(Ordering::Acquire))
}
#[cfg(all(any(test, feature = "test-support"), unix))]
pub fn revision(source: Source) -> u64 {
    REVISIONS[source.index()].load(Ordering::Acquire)
}
