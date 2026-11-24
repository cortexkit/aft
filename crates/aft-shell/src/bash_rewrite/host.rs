use crate::{
    pinned_config::PinnedConfig,
    protocol::{RawRequest, Response},
};
use std::path::{Path, PathBuf};

/// Context policy and dispatch used by the rewrite rules.
#[doc(hidden)]
pub trait RewriteHost: PinnedConfig {
    fn run_rewritten(&self, request: &RawRequest) -> Response;
    fn cache_root(&self) -> Option<PathBuf>;
    fn hashline_effective(&self, root: PathBuf, session: &str) -> bool;
    fn ignores_directory(&self, path: &Path) -> bool;
}
