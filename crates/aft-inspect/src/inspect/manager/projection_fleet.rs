// Projection snapshots are useful only as whole-root splice bases. Bound their
// aggregate residency rather than multiplying a per-root cap by every manager.

pub(super) type ProjectionSlot = Mutex<Option<CachedCallgraphProjection>>;

struct ProjectionFleetEntry {
    slot: Weak<ProjectionSlot>,
    bytes: u64,
}

#[derive(Default)]
pub(super) struct ProjectionFleet {
    entries: HashMap<PathBuf, ProjectionFleetEntry>,
    bytes: u64,
    drops: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadCodeSnapshotCensus {
    pub roots: usize,
    pub bytes: u64,
    pub drops: u64,
}

impl ProjectionFleet {
    pub(super) fn forget(&mut self, root: &Path) {
        if let Some(entry) = self.entries.remove(root) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
    }

    fn census(&mut self) -> DeadCodeSnapshotCensus {
        let stale = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.slot.upgrade().is_none())
            .map(|(root, _)| root.clone())
            .collect::<Vec<_>>();
        for root in stale {
            self.forget(&root);
        }
        DeadCodeSnapshotCensus {
            roots: self.entries.len(),
            bytes: self.bytes,
            drops: self.drops,
        }
    }
}

pub(super) fn projection_fleet() -> &'static Mutex<ProjectionFleet> {
    static FLEET: OnceLock<Mutex<ProjectionFleet>> = OnceLock::new();
    FLEET.get_or_init(|| Mutex::new(ProjectionFleet::default()))
}

pub(crate) fn dead_code_snapshot_census() -> DeadCodeSnapshotCensus {
    projection_fleet()
        .lock()
        .map(|mut fleet| fleet.census())
        .unwrap_or(DeadCodeSnapshotCensus {
            roots: 0,
            bytes: 0,
            drops: 0,
        })
}
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, Weak};

use super::CachedCallgraphProjection;
