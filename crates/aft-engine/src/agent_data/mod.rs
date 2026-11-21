//! Session-owned undo and checkpoint data carried through engram's session door.

pub mod binding;
pub mod door;
pub mod native;
pub mod participant;
pub mod registry;
pub(crate) mod snapshot;
pub mod transport;

pub use snapshot::ImportedGuard;
pub use snapshot::{DropCounts, ExpectedState, FileRecord, SessionSnapshot, SnapshotGroup};

use cortexkit_project_identity::{EnumerateReply, ResolveReply};
use std::path::Path;

/// Implementations must distinguish registry outages from a reachable registry
/// with no matching checkout. Outages abort the operation, never drop its data.
pub trait ProjectRegistry: Send + Sync + std::fmt::Debug {
    fn resolve(&self, path: &Path) -> Result<ResolveReply, String>;
    fn enumerate(&self) -> Result<EnumerateReply, String>;
}

// Advertising this role makes every agent move wait for AFT. Keep discovery off
// until the daemon session fence and atomic publication are connected and
// verified. Enabling discovery before those checks would block agent moves.
pub const CATALOG_ENABLED: bool = false;

#[cfg(test)]
mod door_tests;
#[cfg(test)]
mod tests;
