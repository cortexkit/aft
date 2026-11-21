//! Versioned metadata for the bash rewrite dispatch surface.
//!
//! Keeping these identifiers next to the dispatch implementation gives the
//! differential corpus a closed vocabulary. The corpus can therefore name the
//! behavior it covers without depending on Rust type names or log messages.

use std::fs;
#[cfg(not(test))]
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRole {
    Accept,
    Decline,
    Native,
    Sandbox,
}

impl ControlRole {
    pub const fn id(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Native => "native",
            Self::Sandbox => "sandbox",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionClass {
    pub id: &'static str,
    pub rule_id: &'static str,
    pub baseline_source: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchInventoryEntry {
    pub id: &'static str,
    pub rule_id: Option<&'static str>,
    pub role: ControlRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BypassInventoryEntry {
    pub id: &'static str,
    pub rule_id: Option<&'static str>,
    pub reason: &'static str,
}

/// The seven production rules. `grep` and `rg` intentionally have separate
/// rule IDs but share the `grep_request` builder.
pub const RULE_INVENTORY: &[(&str, &str)] = &[
    ("grep", "grep_request"),
    ("rg", "grep_request"),
    ("find", "find_request"),
    ("cat", "cat_read_request"),
    ("cat_append", "append_request"),
    ("sed", "sed_request"),
    ("ls", "ls_request"),
];

pub const DECISION_CLASSES: &[DecisionClass] = &[
    DecisionClass {
        id: "dc.grep.decline.v1",
        rule_id: "grep",
        baseline_source: "src/bash_rewrite/rules.rs::grep_request",
    },
    DecisionClass {
        id: "dc.rg.decline.v1",
        rule_id: "rg",
        baseline_source: "src/bash_rewrite/rules.rs::grep_request",
    },
    DecisionClass {
        id: "dc.find.decline.v1",
        rule_id: "find",
        baseline_source: "src/bash_rewrite/rules.rs::find_request",
    },
    DecisionClass {
        id: "dc.cat.decline.v1",
        rule_id: "cat",
        baseline_source: "src/bash_rewrite/rules.rs::cat_read_request",
    },
    DecisionClass {
        id: "dc.cat_append.decline.v1",
        rule_id: "cat_append",
        baseline_source: "src/bash_rewrite/rules.rs::append_request",
    },
    DecisionClass {
        id: "dc.sed.decline.v1",
        rule_id: "sed",
        baseline_source: "src/bash_rewrite/rules.rs::sed_request",
    },
    DecisionClass {
        id: "dc.ls.decline.v1",
        rule_id: "ls",
        baseline_source: "src/bash_rewrite/rules.rs::ls_request",
    },
    DecisionClass {
        id: "dc.grep.accept.v1",
        rule_id: "grep",
        baseline_source: "src/commands/grep.rs::handle_grep",
    },
    DecisionClass {
        id: "dc.rg.accept.v1",
        rule_id: "rg",
        baseline_source: "src/commands/grep.rs::handle_grep",
    },
    DecisionClass {
        id: "dc.find.accept.v1",
        rule_id: "find",
        baseline_source: "src/commands/glob.rs::handle_glob",
    },
    DecisionClass {
        id: "dc.cat.accept.v1",
        rule_id: "cat",
        baseline_source: "src/commands/read.rs::handle_read",
    },
    DecisionClass {
        id: "dc.cat_append.accept.v1",
        rule_id: "cat_append",
        baseline_source: "src/commands/edit_match.rs::handle_edit_match",
    },
    DecisionClass {
        id: "dc.sed.accept.v1",
        rule_id: "sed",
        baseline_source: "src/commands/read.rs::handle_read",
    },
    DecisionClass {
        id: "dc.ls.accept.v1",
        rule_id: "ls",
        baseline_source: "src/commands/read.rs::handle_read",
    },
];

/// The generated branch table is deliberately explicit. A new production arm
/// must add an entry here before a corpus change can claim coverage.
pub const BRANCH_INVENTORY: &[BranchInventoryEntry] = &[
    BranchInventoryEntry {
        id: "grep.accept",
        rule_id: Some("grep"),
        role: ControlRole::Accept,
    },
    BranchInventoryEntry {
        id: "grep.decline",
        rule_id: Some("grep"),
        role: ControlRole::Decline,
    },
    BranchInventoryEntry {
        id: "rg.accept",
        rule_id: Some("rg"),
        role: ControlRole::Accept,
    },
    BranchInventoryEntry {
        id: "rg.decline",
        rule_id: Some("rg"),
        role: ControlRole::Decline,
    },
    BranchInventoryEntry {
        id: "find.accept",
        rule_id: Some("find"),
        role: ControlRole::Accept,
    },
    BranchInventoryEntry {
        id: "find.decline",
        rule_id: Some("find"),
        role: ControlRole::Decline,
    },
    BranchInventoryEntry {
        id: "cat.accept",
        rule_id: Some("cat"),
        role: ControlRole::Accept,
    },
    BranchInventoryEntry {
        id: "cat.decline",
        rule_id: Some("cat"),
        role: ControlRole::Decline,
    },
    BranchInventoryEntry {
        id: "cat_append.accept",
        rule_id: Some("cat_append"),
        role: ControlRole::Accept,
    },
    BranchInventoryEntry {
        id: "cat_append.decline",
        rule_id: Some("cat_append"),
        role: ControlRole::Decline,
    },
    BranchInventoryEntry {
        id: "sed.accept",
        rule_id: Some("sed"),
        role: ControlRole::Accept,
    },
    BranchInventoryEntry {
        id: "sed.decline",
        rule_id: Some("sed"),
        role: ControlRole::Decline,
    },
    BranchInventoryEntry {
        id: "ls.accept",
        rule_id: Some("ls"),
        role: ControlRole::Accept,
    },
    BranchInventoryEntry {
        id: "ls.decline",
        rule_id: Some("ls"),
        role: ControlRole::Decline,
    },
    BranchInventoryEntry {
        id: "dispatch.native.no_rule",
        rule_id: None,
        role: ControlRole::Native,
    },
    BranchInventoryEntry {
        id: "dispatch.native.sandbox",
        rule_id: None,
        role: ControlRole::Sandbox,
    },
    BranchInventoryEntry {
        id: "dispatch.native.non_root_workdir",
        rule_id: None,
        role: ControlRole::Native,
    },
];

pub const BYPASS_INVENTORY: &[BypassInventoryEntry] = &[
    BypassInventoryEntry {
        id: "bypass.unsupported_shape",
        rule_id: None,
        reason: "the command is outside the seven-rule accepted shape surface",
    },
    BypassInventoryEntry {
        id: "bypass.external_path",
        rule_id: None,
        reason: "the internal handler cannot represent an external path safely",
    },
    BypassInventoryEntry {
        id: "bypass.non_root_workdir",
        rule_id: None,
        reason: "rewrite paths are project-root relative, while bash honors cwd",
    },
    BypassInventoryEntry {
        id: "bypass.sandbox",
        rule_id: None,
        reason: "native sandboxing must own process execution",
    },
];

pub const SEMANTIC_DIMENSIONS: &[&str] = &[
    "shell-tokenization",
    "path-resolution",
    "working-directory",
    "hidden-files",
    "ordering",
    "match-completeness",
    "exit-status",
    "error-outcome",
    "content-limit",
    "mutation",
    "append-no-double-apply",
    "locale",
    "environment-path",
    "handler-lifecycle",
];

pub const COMPARISON_BASES: &[&str] = &[
    "bytes",
    "ls-entry-set",
    "ls-entry-sequence",
    "find-path-set",
    "grep-match-set",
    "grep-value-multiset",
];

pub const PRESENTATION_NORMALIZATIONS: &[&str] = &["footer-removal", "gutter-removal"];
pub const EXPECTATIONS: &[&str] = &["truncation-disclosed", "characterization-only"];

pub fn rule_inventory() -> &'static [(&'static str, &'static str)] {
    RULE_INVENTORY
}

pub fn branch_inventory() -> &'static [BranchInventoryEntry] {
    BRANCH_INVENTORY
}

pub fn bypass_inventory() -> &'static [BypassInventoryEntry] {
    BYPASS_INVENTORY
}

pub fn rule_exists(rule_id: &str) -> bool {
    RULE_INVENTORY.iter().any(|(id, _)| *id == rule_id)
}

pub fn decision_class(id: &str) -> Option<&'static DecisionClass> {
    DECISION_CLASSES.iter().find(|class| class.id == id)
}

pub fn branch_exists(id: &str) -> bool {
    BRANCH_INVENTORY.iter().any(|branch| branch.id == id)
}

pub fn semantic_dimension_exists(id: &str) -> bool {
    SEMANTIC_DIMENSIONS.contains(&id)
}

#[cfg(not(test))]
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR must be set at runtime by Cargo or nextest")
}

pub fn baseline_source_resolves(source: &str) -> bool {
    let Some((file, symbol)) = source.split_once("::") else {
        return false;
    };
    if file.is_empty() || symbol.is_empty() || symbol.contains("::") {
        return false;
    }
    // Only tests comparing rewritten commands with native Bash validate source
    // locations; shipped rewrite dispatch does not. Relocated unit tests must
    // inspect the consumer's source files.
    #[cfg(test)]
    let manifest_dir = crate::test_support::manifest_dir();
    #[cfg(not(test))]
    let manifest_dir = manifest_dir();
    let path = manifest_dir.join(file);
    path.is_file()
        && fs::read_to_string(path)
            .ok()
            .is_some_and(|source| source.contains(symbol))
}

pub fn validate_catalog() -> Result<(), String> {
    for class in DECISION_CLASSES {
        if !rule_exists(class.rule_id) {
            return Err(format!("decision class {} names unknown rule", class.id));
        }
        if !baseline_source_resolves(class.baseline_source) {
            return Err(format!(
                "decision class {} has unresolved baseline source {}",
                class.id, class.baseline_source
            ));
        }
    }
    for branch in BRANCH_INVENTORY {
        if let Some(rule_id) = branch.rule_id {
            if !rule_exists(rule_id) {
                return Err(format!("branch {} names unknown rule", branch.id));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_sources_and_rule_counts_are_closed() {
        validate_catalog().expect("catalog sources resolve");
        assert_eq!(RULE_INVENTORY.len(), 7);
        assert_eq!(RULE_INVENTORY[0].1, RULE_INVENTORY[1].1);
        assert_eq!(RULE_INVENTORY[0].1, "grep_request");
    }

    #[test]
    fn reserved_control_roles_are_stable() {
        assert_eq!(ControlRole::Accept.id(), "accept");
        assert_eq!(ControlRole::Decline.id(), "decline");
        assert_eq!(ControlRole::Native.id(), "native");
        assert_eq!(ControlRole::Sandbox.id(), "sandbox");
    }
}
