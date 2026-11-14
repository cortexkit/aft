//! The folder the graph writer spills into. It is one builder's folder in
//! the build's shared workspace ([`crate::family::BuildScratch`]), which
//! owns the folder's deletion, the checkout's build lock, disk admission and
//! the sweep of workspaces whose build is no longer live.

use std::path::Path;

/// The graph writer only needs a private directory and its lifetime guard.
pub trait Scratch: std::fmt::Debug + Send {
    fn path(&self) -> &Path;
}

impl<E> Scratch for crate::family::ScratchDir<E> {
    fn path(&self) -> &Path {
        crate::family::ScratchDir::path(self)
    }
}

/// A graph folder in a workspace of its own under `dir`, for tests.
#[cfg(test)]
pub(crate) fn for_test(dir: &Path) -> Box<dyn Scratch> {
    let workspace = std::sync::Arc::new(
        crate::family::BuildScratch::create_unattached(dir, |_| Ok::<_, ()>(())).unwrap(),
    );
    Box::new(workspace.subdir("graph").unwrap())
}
