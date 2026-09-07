//! Drift guard: every native bridge command the plugins call through their
//! bash bridge must be admitted by the subc gate. The gate is fail-closed, so
//! an unlisted command is rejected before it can reach dispatch on a bound
//! route.
//!
//! The required set is derived from production plugin source instead of being
//! maintained as a second hand-written list. When a plugin adds a literal
//! `callBashBridge` command, this test makes the admission decision explicit:
//! add a rationale to `is_subc_native_plumbing_tool` or route the command
//! through the agent tool manifest.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use aft::subc::is_tool_call_admitted_for_test;

/// Hashline has a separate callToolCall path and therefore remains covered by
/// a dispatch-arm check in addition to the derived bash bridge set below.
const PLUGIN_HASHLINE_NATIVE_CALLS: &[&str] = &["hashline_preflight"];
const PLUGIN_NAMES: &[&str] = &["opencode-plugin", "pi-plugin"];
const MANAGEMENT_OPERATIONS: &[&str] = &["health.digest", "memory.census"];

fn plugin_bridge_calls(plugin: &str) -> BTreeSet<String> {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages")
        .join(plugin)
        .join("src");
    assert!(
        source_root.is_dir(),
        "plugin source directory is missing: {}",
        source_root.display()
    );

    let mut names = BTreeSet::new();
    collect_plugin_sources(&source_root, &mut names);
    names
}

fn collect_plugin_sources(path: &Path, names: &mut BTreeSet<String>) {
    let entries = fs::read_dir(path).unwrap_or_else(|error| {
        panic!(
            "failed to read plugin source directory {}: {error}",
            path.display()
        )
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "failed to read plugin source entry in {}: {error}",
                path.display()
            )
        });
        let entry_path = entry.path();
        if entry_path.is_dir() {
            // Tests may use the same helper names but are not production bridge
            // call sites, so only production source under each plugin's src is scanned.
            if entry_path.file_name().and_then(|name| name.to_str()) != Some("__tests__") {
                collect_plugin_sources(&entry_path, names);
            }
        } else if is_plugin_source_file(&entry_path) {
            let source = fs::read_to_string(&entry_path).unwrap_or_else(|error| {
                panic!(
                    "failed to read plugin source {}: {error}",
                    entry_path.display()
                )
            });
            names.extend(extract_call_bash_bridge_names(&source));
        }
    }
}

fn is_plugin_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "ts" | "tsx")
    )
}

fn extract_call_bash_bridge_names(source: &str) -> BTreeSet<String> {
    const MARKER: &str = "callBashBridge";
    let mut names = BTreeSet::new();
    let mut search_from = 0;

    while let Some(relative_start) = source[search_from..].find(MARKER) {
        let marker_start = search_from + relative_start;
        let after_marker = marker_start + MARKER.len();
        let Some(relative_open) = source[after_marker..].find('(') else {
            break;
        };
        let open = after_marker + relative_open;
        let Some(close) = matching_parenthesis(source, open) else {
            break;
        };
        names.extend(extract_bash_string_literals(&source[open + 1..close]));
        search_from = close + 1;
    }

    names
}

fn matching_parenthesis(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut index = open;

    while index < bytes.len() {
        let byte = bytes[index];
        if line_comment {
            if byte == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment {
            if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }

        match byte {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                line_comment = true;
                index += 2;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                block_comment = true;
                index += 2;
            }
            b'\'' | b'"' | b'`' => {
                quote = Some(byte);
                index += 1;
            }
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }

    None
}

fn extract_bash_string_literals(arguments: &str) -> BTreeSet<String> {
    let bytes = arguments.as_bytes();
    let mut names = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        let quote = match bytes[index] {
            b'\'' | b'"' => bytes[index],
            _ => {
                index += 1;
                continue;
            }
        };
        let start = index + 1;
        index = start;
        let mut escaped = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == quote {
                let literal = &arguments[start..index];
                if literal == "bash" || literal.starts_with("bash_") {
                    names.insert(literal.to_string());
                }
                index += 1;
                break;
            }
            index += 1;
        }
    }
    names
}

#[test]
fn plugin_call_bash_bridge_names_are_admitted_by_subc_gate() {
    let mut all_names = BTreeSet::new();
    for plugin in PLUGIN_NAMES {
        let names = plugin_bridge_calls(plugin);
        assert!(
            !names.is_empty(),
            "no callBashBridge command literals found in {plugin} production sources"
        );
        let rejected: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|name| !is_tool_call_admitted_for_test(name))
            .collect();
        assert!(
            rejected.is_empty(),
            "{plugin} callBashBridge commands rejected by the subc fail-closed gate \
             (add to is_subc_native_plumbing_tool with rationale): {rejected:?}"
        );
        all_names.extend(names);
    }

    // This sanity check proves the source-derived scan covers the regex
    // validation call that would otherwise be hidden by a stale hand-list.
    assert!(
        all_names.contains("bash_regex_match"),
        "production plugin sources did not yield the bash_regex_match bridge call"
    );
}

#[test]
fn management_operations_stay_out_of_schema_derived_agent_surfaces() {
    let schema: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!("../../src/subc_tool_schemas.json"))
            .expect("embedded subc tool schema artifact");
    for operation in MANAGEMENT_OPERATIONS {
        assert!(
            !schema.contains_key(*operation),
            "management operation {operation:?} must not enter the schema-derived agent manifest"
        );
        assert!(
            !is_tool_call_admitted_for_test(operation),
            "management operation {operation:?} must not pass the tool-provider route gate"
        );
    }

    let manifest_source = include_str!("../../src/subc/manifest.rs");
    assert!(manifest_source.contains("ProviderRole::ManagementSurface"));
    assert!(manifest_source.contains("HEALTH_DIGEST_OPERATION.to_string()"));
    assert!(manifest_source.contains("MEMORY_CENSUS_OPERATION.to_string()"));
}

#[test]
fn plugin_hashline_native_calls_have_main_dispatch_arms() {
    let main_dispatch = include_str!("../../src/main.rs");
    for name in PLUGIN_HASHLINE_NATIVE_CALLS {
        let dispatch_arm = format!("\"{name}\" =>");
        assert!(
            main_dispatch.contains(&dispatch_arm),
            "plugin hashline plumbing command {name:?} has no main dispatch arm"
        );
    }
}
