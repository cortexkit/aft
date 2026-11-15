//! Managed child paths supplied by the shell owner when the process starts.

use crate::config::Config;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

type ManagedReadPaths = fn(&Config, &Path) -> Vec<PathBuf>;
static MANAGED_READ_PATHS: OnceLock<ManagedReadPaths> = OnceLock::new();

pub(crate) fn register(paths: ManagedReadPaths) {
    let _ = MANAGED_READ_PATHS.set(paths);
}

pub(crate) fn managed_read_paths(config: &Config, storage: &Path) -> Vec<PathBuf> {
    // Missing registration adds no read exceptions. It can deny a managed
    // child path, but must never authorize a path the owning shell did not supply.
    MANAGED_READ_PATHS
        .get()
        .map(|paths| paths(config, storage))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn registered() -> bool {
    MANAGED_READ_PATHS.get().is_some()
}
