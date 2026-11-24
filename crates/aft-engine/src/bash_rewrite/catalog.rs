pub use aft_shell::bash_rewrite::catalog::*;
use std::fs;
#[cfg(not(test))]
use std::path::PathBuf;

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
    // Catalog paths are package-relative. Handler sources stay in the upper
    // package, while the rewrite rules move with this catalog.
    let package = if file.starts_with("src/commands/") {
        "aft-tools"
    } else {
        "aft-shell"
    };
    let path = manifest_dir.join("..").join(package).join(file);
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
}
