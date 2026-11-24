use std::path::Path;

use super::host::IndexHost;
use crate::read_policy::ReadPolicy;

/// Observations used to describe why a call graph query could not run.
#[doc(hidden)]
pub trait CallgraphQueryHost: IndexHost + ReadPolicy {
    fn callgraph_path_is_ignored(&self, path: &Path) -> bool;
    fn note_callgraph_query(&self, tool: &str, service_ms: u64, status: &str);
    fn is_worktree_bridge(&self) -> bool;
    fn has_callgraph_project_root(&self) -> bool;
}
