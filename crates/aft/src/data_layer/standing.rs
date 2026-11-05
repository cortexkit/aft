//! Standing index roots and `aft index` on the data layer.
//!
//! With the data layer on, the text (`search`) and call-graph (`callgraph`)
//! kinds of a configured standing root are the root's checkout in the data
//! layer: one build publishes both, through the family store's own writer,
//! and nothing is written to today's search cache or call-graph store. The
//! meaning (`semantic`) kind keeps today's semantic index until it moves.
//!
//! A standing root has no watcher, so each standing pass binds its checkout
//! again: the bind's scan compares every file with the publication, and the
//! build that follows, if any file differs, scans and publishes the whole
//! checkout. Either one is the strict current-state proof that clears the
//! root's durable strict-verification flag for those kinds.

use std::path::Path;

use crate::config::IndexKind;

/// Whether the data layer builds a standing root's `kind`: its text
/// (`search`) and its call graph (`callgraph`), which one build publishes.
#[must_use]
pub const fn builds(kind: IndexKind) -> bool {
    matches!(kind, IndexKind::Search | IndexKind::Callgraph)
}

/// Brings the data layer's checkout at `root`, with its family store under
/// `storage`, up to date with the files on disk, in the foreground, and
/// returns whether it now holds exactly them: the bind's scan found
/// nothing changed since the publication, or a build scanned the whole
/// checkout and published. `still_wanted` is asked before the work and
/// again after it: once it returns `false`, so does this, even if the work
/// published. A standing pass passes one that turns `false` when a session
/// binds the root or the pass's job is cancelled, so such a pass never
/// clears the root's strict-verification flag.
pub(crate) fn verify_current(root: &Path, storage: &Path, still_wanted: impl Fn() -> bool) -> bool {
    if !still_wanted() {
        return false;
    }
    let current = super::surfaces::index_root(root, storage).is_ok();
    current && still_wanted()
}
