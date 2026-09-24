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

/// Read a tier file if present; returns None when the file is absent (case has
/// no tier at that level).
fn read_tier(dir: &Path, filename: &str, tier: &str) -> Option<ConfigTier> {
    let path = dir.join(filename);
    let doc = fs::read_to_string(&path).ok()?;
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
    let result = resolve_config_for_harness(&tiers, Some(&harness));

    let golden: Value = serde_json::from_str(
        &fs::read_to_string(dir.join("expected.json")).expect("read expected.json"),
    )
    .expect("parse expected.json");

    // A rejected TypeScript load must be rejected by Rust with the same codes.
    if let Some(rejected) = golden.get("rejected") {
        let want: Vec<String> = serde_json::from_value(rejected.clone()).expect("rejected codes");
        return (result.errors != want).then(|| {
            format!(
                "case `{case}`: rejection mismatch\n  resolved errors: {:?}\n  expected: {want:?}",
                result.errors
            )
        });
    }
    if !result.errors.is_empty() {
        return Some(format!(
            "case `{case}`: Rust rejected a config TypeScript accepted: {:?}",
            result.errors
        ));
    }
    // The golden must carry the registration list and every index switch
    // itself; otherwise the overlay below would silently fill them from
    // `Config::default()` and the comparison could pass for the wrong reason.
    if let Err(errors) = validate_resolved_config(&golden) {
        return Some(format!("case `{case}`: golden is not a resolved config: {errors:?}"));
    }
    let resolved = result.config;
    let mut resolved_json = serde_json::to_value(&resolved).expect("serialize resolved config");

    // Expected = Config::default() with the captured TS configure params overlaid.
    let mut golden = golden;
    fill_server_defaults(&mut golden);
    let mut want_json = serde_json::to_value(Config::default()).expect("serialize default config");
    overlay(&mut want_json, &golden);

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
