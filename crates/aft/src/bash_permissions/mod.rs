//! Bash permission scanner for hoisted bash. Phase 0 stub; Phase 1 Track C fills in.
//!
//! Ports OpenCode's tree-sitter-based permission scan that walks the parsed
//! command tree to identify sub-commands that touch external directories or
//! match permission rules.

pub mod arity;
pub mod scan;

use crate::context::AppContext;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAsk {
    pub kind: PermissionKind,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
