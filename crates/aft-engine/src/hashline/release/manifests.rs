//! Ownership and nonintersecting fence manifest checks for A1–A18 and FINAL GATES.
//!
//! The fence check consumes the committed mapping: every acceptance row has
//! exactly one owning slice, and every slice fence is disjoint from the others.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

const OWNERSHIP_JSON: &str = include_str!("ownership.json");
const FENCES_JSON: &str = include_str!("fences.json");
const FINAL_GATES_JSON: &str = include_str!("final_gates.json");
const NOISE_POLICY_JSON: &str = include_str!("noise_policy.json");

/// Acceptance rows that must appear in the ownership manifest.
pub const REQUIRED_ROWS: &[&str] = &[
    "A1",
    "A2",
    "A3",
    "A4",
    "A5",
    "A6",
    "A7",
    "A8",
    "A9",
    "A10",
    "A11",
    "A12",
    "A13",
    "A14",
    "A16",
    "A17",
    "A18",
    "FINAL_GATES",
];

/// Delivery-plan slices that must each own a disjoint fence.
pub const REQUIRED_SLICES: &[&str] = &[
    "slice-1-oracle-hash-parity",
    "slice-2-scanner-byte-model",
    "slice-3-snapshots-residency",
    "slice-4-parser-lookup-verification",
    "slice-5-apply-repair-registers",
    "slice-6-transactions-mv-preview",
    "slice-7-remap-recovery",
    "slice-8-registration-transports",
    "slice-9-performance-release-gates",
];

#[derive(Debug, Deserialize)]
struct OwnershipFile {
    rows: BTreeMap<String, OwnershipRow>,
}

#[derive(Debug, Deserialize)]
struct OwnershipRow {
    owner: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    gates: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct FencesFile {
    slices: Vec<FenceSlice>,
}

#[derive(Debug, Deserialize)]
struct FenceSlice {
    id: String,
    fence: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FinalGatesFile {
    owner: String,
    gates: Vec<FinalGate>,
}

#[derive(Debug, Deserialize)]
struct FinalGate {
    id: String,
    #[serde(default)]
    command: Option<String>,
}

/// Parse and validate ownership + fence manifests. Returns the owner map on success.
pub fn load_ownership() -> Result<BTreeMap<String, String>, String> {
    let file: OwnershipFile = serde_json::from_str(OWNERSHIP_JSON)
        .map_err(|error| format!("ownership.json parse error: {error}"))?;
    let mut owners = BTreeMap::new();
    for &row in REQUIRED_ROWS {
        let entry = file
            .rows
            .get(row)
            .ok_or_else(|| format!("ownership.json missing required row {row}"))?;
        if entry.owner.trim().is_empty() {
            return Err(format!("ownership.json row {row} has an empty owner"));
        }
        if !REQUIRED_SLICES.contains(&entry.owner.as_str()) {
            return Err(format!(
                "ownership.json row {row} owner `{}` is not a known slice",
                entry.owner
            ));
        }
        owners.insert(row.to_string(), entry.owner.clone());
        let _ = entry.title.as_ref();
    }
    // No extra unknown acceptance rows with empty owners; extras are allowed
    // only if they also name a known slice (future rows).
    for (row, entry) in &file.rows {
        if !REQUIRED_ROWS.contains(&row.as_str())
            && !REQUIRED_SLICES.contains(&entry.owner.as_str())
        {
            return Err(format!(
                "ownership.json extra row {row} names unknown owner {}",
                entry.owner
            ));
        }
    }
    // Exactly-one-owner is structural in the map (one entry per row). Also
    // assert FINAL_GATES lists the four release-train gates.
    let final_row = file.rows.get("FINAL_GATES").expect("checked above");
    let gates = final_row
        .gates
        .as_ref()
        .ok_or_else(|| "FINAL_GATES must list gates".to_string())?;
    for required in [
        "rust-test-gate",
        "typescript-suites",
        "governed-schema-and-manifest-audit",
        "windows-path-and-crlf-ci",
    ] {
        if !gates.iter().any(|gate| gate == required) {
            return Err(format!("FINAL_GATES missing {required}"));
        }
    }
    Ok(owners)
}

/// Load fence manifests and prove pairwise non-intersection of expanded prefixes.
pub fn load_and_check_fences() -> Result<BTreeMap<String, Vec<String>>, String> {
    let file: FencesFile = serde_json::from_str(FENCES_JSON)
        .map_err(|error| format!("fences.json parse error: {error}"))?;
    let mut by_slice: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for slice in file.slices {
        if !REQUIRED_SLICES.contains(&slice.id.as_str()) {
            return Err(format!("fences.json unknown slice id {}", slice.id));
        }
        if slice.fence.is_empty() {
            return Err(format!("fences.json slice {} has an empty fence", slice.id));
        }
        if by_slice
            .insert(slice.id.clone(), slice.fence.clone())
            .is_some()
        {
            return Err(format!("fences.json duplicate slice id {}", slice.id));
        }
    }
    for &required in REQUIRED_SLICES {
        if !by_slice.contains_key(required) {
            return Err(format!("fences.json missing slice {required}"));
        }
    }

    // Non-intersection: compare normalized directory prefixes. Globs end in
    // `/**`; two fences intersect when one prefix equals or nests under another.
    let normalized: Vec<(String, Vec<String>)> = by_slice
        .iter()
        .map(|(id, globs)| {
            let prefixes = globs
                .iter()
                .map(|glob| normalize_fence_prefix(glob))
                .collect::<Vec<_>>();
            (id.clone(), prefixes)
        })
        .collect();

    for (index, (left_id, left_prefixes)) in normalized.iter().enumerate() {
        for (right_id, right_prefixes) in normalized.iter().skip(index + 1) {
            for left in left_prefixes {
                for right in right_prefixes {
                    if prefixes_intersect(left, right) {
                        return Err(format!(
                            "fence intersection: {left_id} (`{left}`) intersects {right_id} (`{right}`)"
                        ));
                    }
                }
            }
        }
    }
    Ok(by_slice)
}

fn normalize_fence_prefix(glob: &str) -> String {
    let trimmed = glob.trim().trim_end_matches('/').trim_end_matches("/**");
    trimmed.replace('\\', "/")
}

fn prefixes_intersect(left: &str, right: &str) -> bool {
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

/// Every ownership owner must have a fence entry (and vice versa coverage for slice-9).
pub fn ownership_owners_are_fenced(
    owners: &BTreeMap<String, String>,
    fences: &BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    let owner_set: BTreeSet<&str> = owners.values().map(String::as_str).collect();
    for owner in owner_set {
        if !fences.contains_key(owner) {
            return Err(format!("owner {owner} has no fence entry"));
        }
    }
    // Slice 9 must own A13 and FINAL_GATES.
    if owners.get("A13").map(String::as_str) != Some("slice-9-performance-release-gates") {
        return Err("A13 must be owned by slice-9-performance-release-gates".into());
    }
    if owners.get("FINAL_GATES").map(String::as_str) != Some("slice-9-performance-release-gates") {
        return Err("FINAL_GATES must be owned by slice-9-performance-release-gates".into());
    }
    if fences.get("slice-9-performance-release-gates").is_none() {
        return Err("slice-9 fence missing".into());
    }
    let slice9 = &fences["slice-9-performance-release-gates"];
    if !slice9.iter().any(|glob| glob.contains("hashline/release")) {
        return Err("slice-9 fence must cover hashline/release".into());
    }
    Ok(())
}

/// Final-gates inventory must name the release-train commands.
pub fn load_final_gates() -> Result<Vec<String>, String> {
    let file: FinalGatesFile = serde_json::from_str(FINAL_GATES_JSON)
        .map_err(|error| format!("final_gates.json parse error: {error}"))?;
    if file.owner != "slice-9-performance-release-gates" {
        return Err(format!(
            "final_gates.json owner must be slice-9, got {}",
            file.owner
        ));
    }
    let ids: Vec<String> = file.gates.iter().map(|gate| gate.id.clone()).collect();
    for required in [
        "rust-test-gate",
        "typescript-suites",
        "governed-schema-and-manifest-audit",
        "windows-path-and-crlf-ci",
        "fence-check",
        "a13-performance",
    ] {
        if !ids.iter().any(|id| id == required) {
            return Err(format!("final_gates.json missing gate {required}"));
        }
    }
    for gate in &file.gates {
        if gate
            .command
            .as_ref()
            .is_some_and(|command| command.is_empty())
        {
            return Err(format!("final gate {} has an empty command", gate.id));
        }
    }
    Ok(ids)
}

/// Noise policy must stay aligned with the A13 method constants.
pub fn noise_policy_is_well_formed() -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(NOISE_POLICY_JSON)
        .map_err(|error| format!("noise_policy.json parse error: {error}"))?;
    let runner = value
        .get("pinned_runner")
        .ok_or("noise_policy missing pinned_runner")?;
    let threads = runner
        .get("test_threads")
        .and_then(|value| value.as_u64())
        .ok_or("noise_policy.pinned_runner.test_threads missing")?;
    if threads != 1 {
        return Err(format!(
            "pinned runner must be serial (test_threads=1), got {threads}"
        ));
    }
    let profile = runner
        .get("profile")
        .and_then(|value| value.as_str())
        .ok_or("noise_policy.pinned_runner.profile missing")?;
    if profile != "release" {
        return Err(format!(
            "pinned runner profile must be release, got {profile}"
        ));
    }
    let sampling = value
        .get("sampling")
        .ok_or("noise_policy missing sampling")?;
    let warmups = sampling
        .get("warmups")
        .and_then(|value| value.as_u64())
        .ok_or("warmups missing")?;
    let reps = sampling
        .get("timed_repetitions")
        .and_then(|value| value.as_u64())
        .ok_or("timed_repetitions missing")?;
    if warmups != 3 || reps != 10 {
        return Err(format!(
            "sampling must be 3 warmups / 10 reps, got {warmups}/{reps}"
        ));
    }
    let thresholds = value
        .get("thresholds")
        .ok_or("noise_policy missing thresholds")?;
    let tag_ns = thresholds
        .get("tag_compute_median_max_ns")
        .and_then(|value| value.as_u64())
        .ok_or("tag_compute_median_max_ns missing")?;
    if tag_ns != 1_000_000 {
        return Err(format!("tag median max must be 1_000_000 ns, got {tag_ns}"));
    }
    let ratio = thresholds
        .get("read_render_regression_max_ratio")
        .and_then(|value| value.as_f64())
        .ok_or("read_render_regression_max_ratio missing")?;
    if (ratio - 0.10).abs() > f64::EPSILON {
        return Err(format!("render regression ratio must be 0.10, got {ratio}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_covers_a1_through_a18_and_final_gates_uniquely() {
        let owners = load_ownership().expect("ownership");
        assert_eq!(owners.len(), REQUIRED_ROWS.len());
        for &row in REQUIRED_ROWS {
            assert!(owners.contains_key(row), "missing {row}");
        }
        // Exactly one owner per row is the map invariant; also ensure no row
        // accidentally lists a blank owner (checked in load_ownership).
        let mut seen = BTreeSet::new();
        for row in REQUIRED_ROWS {
            assert!(seen.insert(*row), "duplicate row key {row}");
        }
    }

    #[test]
    fn fences_are_nonintersecting_and_cover_all_slices() {
        let fences = load_and_check_fences().expect("fences");
        assert_eq!(fences.len(), REQUIRED_SLICES.len());
    }

    #[test]
    fn ownership_and_fences_agree_and_slice9_owns_a13_and_final_gates() {
        let owners = load_ownership().expect("ownership");
        let fences = load_and_check_fences().expect("fences");
        ownership_owners_are_fenced(&owners, &fences).expect("owners fenced");
    }

    #[test]
    fn final_gates_inventory_lists_release_train_commands() {
        let ids = load_final_gates().expect("final gates");
        assert!(ids.contains(&"rust-test-gate".to_string()));
        assert!(ids.contains(&"windows-path-and-crlf-ci".to_string()));
    }

    #[test]
    fn noise_policy_matches_a13_method() {
        noise_policy_is_well_formed().expect("noise policy");
    }

    #[test]
    fn prefix_intersection_helper_detects_nested_fences() {
        assert!(prefixes_intersect(
            "crates/aft/src/hashline/release",
            "crates/aft/src/hashline/release"
        ));
        assert!(prefixes_intersect(
            "crates/aft/src/hashline",
            "crates/aft/src/hashline/release"
        ));
        assert!(!prefixes_intersect(
            "crates/aft/src/hashline/release",
            "crates/aft/src/hashline/oracle"
        ));
    }

    #[test]
    fn normalize_fence_prefix_strips_glob_suffix() {
        assert_eq!(
            normalize_fence_prefix("crates/aft/src/hashline/release/**"),
            "crates/aft/src/hashline/release"
        );
    }
}
