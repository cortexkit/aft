//! Cross-language parity gate for the Rust config resolver.
//!
//! Feeds the golden fixtures captured from the CURRENT TypeScript pipeline
//! (`scripts/capture-config-parity.ts`) through `aft::config_resolve::resolve_config`
//! and asserts the resolved flat `Config` matches what TS would have sent as
//! configure params. Any drift between the Zod resolution and the serde
//! resolution fails here — this is the security/behavior gate for P1.

use std::fs;
use std::path::{Path, PathBuf};

use aft::config::{Config, UserServerDef};
use aft::config_resolve::{resolve_config_for_harness, ConfigTier};
use aft::feature_config::validate_resolved_config;
use aft::harness::Harness;
use serde_json::Value;
use std::str::FromStr;

fn fixtures_root() -> PathBuf {
    crate::helpers::cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join("config_parity")
}

/// Placeholder the capture script writes in front of paths that must be
/// absolute, such as an accepted `storage.cache_dir`. `/aft-cache` is absolute
/// on macOS and Linux but relative on Windows (no drive letter), so one fixture
/// spelling cannot be absolute everywhere.
const ABSOLUTE_ROOT_PLACEHOLDER: &str = "{ABSOLUTE_ROOT}";

/// Replace the placeholder with an absolute root for this platform: empty on
/// Unix (`{ABSOLUTE_ROOT}/aft-cache` becomes `/aft-cache`) and a drive letter
/// on Windows (`C:/aft-cache`). Applied to the tier files and the golden alike,
/// so both sides of the comparison carry the same platform path.
fn platform_fixture_text(text: &str) -> String {
    let root = if cfg!(windows) { "C:" } else { "" };
    text.replace(ABSOLUTE_ROOT_PLACEHOLDER, root)
}

/// Read a tier file if present; returns None when the file is absent (case has
/// no tier at that level).
fn read_tier(dir: &Path, filename: &str, tier: &str) -> Option<ConfigTier> {
    let path = dir.join(filename);
    let doc = platform_fixture_text(&fs::read_to_string(&path).ok()?);
    Some(ConfigTier {
        tier: tier.to_string(),
        source: path.to_string_lossy().to_string(),
        doc,
    })
}

/// Recursively overlay `over` onto `base`: object values are deep-merged,
/// everything else replaces. Mirrors "apply these configure params onto
/// Config::default()".
fn overlay(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(base_map), Value::Object(over_map)) => {
            for (k, v) in over_map {
                overlay(base_map.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (base_slot, over_val) => {
            *base_slot = over_val.clone();
        }
    }
}

/// Sort the `disabled_lsp` array (HashSet-backed → nondeterministic serialize
/// order) so equality is order-independent.
fn normalize(value: &mut Value) {
    if let Value::Object(map) = value {
        if let Some(Value::Array(arr)) = map.get_mut("disabled_lsp") {
            arr.sort_by_key(std::string::ToString::to_string);
        }
    }
}

/// The TS golden omits empty `UserServerDef` fields (extensions/env/
/// initialization_options) that the resolved Config fills with type defaults.
/// Rebuild each golden `lsp_servers` element on top of `UserServerDef::default()`
/// so the comparison reflects the same fully-populated shape the resolver emits
/// — this is a harness fidelity concern, not resolver behavior.
fn fill_server_defaults(golden: &mut Value) {
    let Some(Value::Array(servers)) = golden.get_mut("lsp_servers") else {
        return;
    };
    let default_server = serde_json::to_value(UserServerDef::default()).unwrap();
    for element in servers.iter_mut() {
        let mut filled = default_server.clone();
        overlay(&mut filled, element);
        *element = filled;
    }
}

fn read_harness(dir: &Path) -> Harness {
    let path = dir.join("harness.json");
    let value = fs::read_to_string(path)
        .ok()
        .and_then(|doc| serde_json::from_str::<String>(&doc).ok())
        .unwrap_or_else(|| "opencode".to_string());
    Harness::from_str(&value)
        .unwrap_or_else(|error| panic!("invalid fixture harness {value:?}: {error}"))
}

fn assert_case(dir: &Path) -> Option<String> {
    let case = dir.file_name().unwrap().to_string_lossy().to_string();

    // Build the ordered tier list exactly as the plugin will: user first, then
    // project. Absent files contribute no tier.
    let mut tiers = Vec::new();
    if let Some(user) = read_tier(dir, "user.jsonc", "user") {
        tiers.push(user);
    }
    if let Some(project) = read_tier(dir, "project.jsonc", "project") {
        tiers.push(project);
    }

    let harness = read_harness(dir);
    // The fixtures include retired keys such as `search_index`, `gh_read` and
    // `idle.lsp_ttl_minutes`; both languages translate them in memory.
    let result = resolve_config_for_harness(&tiers, Some(&harness));

    let golden: Value = serde_json::from_str(&platform_fixture_text(
        &fs::read_to_string(dir.join("expected.json")).expect("read expected.json"),
    ))
    .expect("parse expected.json");

    // A rejected TypeScript load must be rejected by Rust with the same codes.
    if let Some(rejected) = golden.get("rejected") {
        let want: Vec<String> = serde_json::from_value(rejected.clone()).expect("rejected codes");
        // The engine names the refused `bash.env` key and the plugin reports
        // the section code; both reject the same candidate for the same
        // reason, so code the engine's message as that section.
        let got: Vec<String> = result
            .errors
            .iter()
            .map(|error| {
                if error.contains("blocked bash.env key") {
                    "invalid_security_config:bash".to_string()
                } else {
                    error.clone()
                }
            })
            .collect();
        return (got != want).then(|| {
            format!(
                "case `{case}`: rejection mismatch\n  resolved errors: {:?}\n  expected: {want:?}",
                result.errors
            )
        });
    }
    if !result.errors.is_empty() {
        // The engine refuses some `bash.env` names the plugin schema accepts.
        // Two reasons, both deliberate: the plugin's project-tier check runs
        // after the shared security-section classifier, so a refused project
        // name is reported only as a member of that section, and the plugin
        // schema cannot tell a user tier from a project tier on its own. The
        // refusal must still name the key, so a fixture cannot pass here by
        // having its environment field dropped silently.
        let refused_here = result
            .errors
            .iter()
            .all(|error| error.contains("blocked bash.env key"));
        if refused_here && case.starts_with("bash_env_") {
            return None;
        }
        return Some(format!(
            "case `{case}`: Rust rejected a config TypeScript accepted: {:?}",
            result.errors
        ));
    }
    // The golden must carry the registration list and every index switch
    // itself; otherwise the overlay below would silently fill them from
    // `Config::default()` and the comparison could pass for the wrong reason.
    if let Err(errors) = validate_resolved_config(&golden) {
        return Some(format!(
            "case `{case}`: golden is not a resolved config: {errors:?}"
        ));
    }
    let resolved = result.config;
    let mut resolved_json = serde_json::to_value(&resolved).expect("serialize resolved config");

    // Expected = Config::default() with the captured TS configure params overlaid.
    let mut golden = golden;
    // A project's `bash.env` is carried as `project_env` on the TypeScript side;
    // the Rust resolver reads the same values from the project tier itself.
    golden
        .as_object_mut()
        .map(|object| object.remove("project_env"));
    fill_server_defaults(&mut golden);
    let mut want_json = serde_json::to_value(Config::default()).expect("serialize default config");
    overlay(&mut want_json, &golden);

    // The engine carries a project's validated `bash.env` in `project_env` for
    // `commands/bash.rs` to apply behind the trust check; it is not part of the
    // configure-params surface both sides compare here.
    resolved_json
        .get_mut("bash")
        .and_then(|bash| bash.as_object_mut())
        .map(|bash| bash.remove("project_env"));
    normalize(&mut resolved_json);
    normalize(&mut want_json);

    if resolved_json == want_json {
        None
    } else {
        let mut paths = Vec::new();
        diff_paths("", &resolved_json, &want_json, &mut paths);
        Some(format!("case `{case}`: differing paths {paths:?}"))
    }
}

/// Collect the JSON paths where two values differ, with both sides.
fn diff_paths(prefix: &str, resolved: &Value, want: &Value, out: &mut Vec<String>) {
    match (resolved, want) {
        (Value::Object(left), Value::Object(right)) => {
            let mut keys: Vec<&String> = left.keys().chain(right.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let path = format!("{prefix}.{key}");
                diff_paths(
                    &path,
                    left.get(key).unwrap_or(&Value::Null),
                    right.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (left, right) if left != right => {
            out.push(format!("{prefix}: resolved={left} expected={right}"));
        }
        _ => {}
    }
}

#[test]
fn config_resolver_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();

    // The pinned baseline IDs are guarded by
    // spec/feature-config/check-parity-baseline.ts; this floor keeps the
    // feature-config cases (legacy mappings and list presence) as well.
    assert!(
        cases.len() >= 98,
        "expected at least 98 parity fixtures after capture regeneration, found {}",
        cases.len()
    );

    let failures: Vec<String> = cases.iter().filter_map(|dir| assert_case(dir)).collect();
    assert!(
        failures.is_empty(),
        "{} parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
