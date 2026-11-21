//! Bash permission scanner for hoisted bash.
//!
//! Ports OpenCode's tree-sitter-based permission scan that walks the parsed
//! command tree to identify sub-commands that touch external directories or
//! match permission rules.

pub mod arity;
pub mod scan;
pub(crate) mod synthetic_load;
#[cfg(unix)]
pub(crate) mod writes;

use crate::context::AppContext;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAsk {
    pub kind: PermissionKind,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Actual command targets, before an optional parent-folder scope is
    /// offered. Retry coverage must not depend on whether that probe succeeded.
    #[serde(skip)]
    pub targets: Vec<crate::access_policy::AccessGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionKind {
    #[serde(rename = "external_directory")]
    ExternalDirectory,
    #[serde(rename = "bash")]
    Bash,
}

/// Scan a bash command and return the list of permission asks needed.
pub fn scan(command: &str, ctx: &AppContext) -> Vec<PermissionAsk> {
    scan::scan(command, ctx)
}

use crate::path_identity::lexical_normalize as normalize_path;
pub(crate) use crate::system_temp::is_system_temp_path;
