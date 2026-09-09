use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
enum FenceRule {
    Exact(&'static str),
    Prefix(&'static str),
}

impl FenceRule {
    fn matches(self, path: &str) -> bool {
        match self {
            Self::Exact(expected) => path == expected,
            Self::Prefix(prefix) => path.starts_with(prefix),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Exact(path) | Self::Prefix(path) => path,
        }
    }
}

#[derive(Clone, Debug)]
struct Slice {
    id: &'static str,
    fence: Vec<FenceRule>,
    landed_paths: Vec<&'static str>,
}

const SEMANTIC_MODULE_PREFIX: &str = "crates/aft/src/commands/semantic_search/";
const ENGINE_FIXTURE_PREFIX: &str = "benchmarks/aft-search/engine-fixtures/";
const ENGINE_TEST_PREFIX: &str = "crates/aft/tests/engine_";
const RANKING_DIRECTORY: &str = "crates/aft/src/ranking";
const AUDIT_PATH: &str = "crates/aft/tests/engine_audit_ownership_test.rs";
const FIRST_SLICE: &str = "A1-seam-comparator-descriptor";

const B_OWNED_EXACT: &[&str] = &[
    "crates/aft/src/query_shape.rs",
    "crates/aft/src/semantic_index.rs",
    "benchmarks/aft-search/fixtures.json",
    "benchmarks/aft-search/real-query-manifest.json",
    "benchmarks/aft-search/real-query-baseline.json",
    "scripts/telemetry/cost-gate.sh",
];

const SEAM_PATHS: &[&str] = &[
    "crates/aft/src/commands/semantic_search.rs",
    "crates/aft/src/commands/semantic_search/mod.rs",
    "crates/aft/src/commands/semantic_search/plan_table.rs",
    "crates/aft/src/commands/semantic_search/generation_token.rs",
];

fn slices() -> Vec<Slice> {
    vec![
        Slice {
            id: FIRST_SLICE,
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/comparator/"),
                FenceRule::Exact("benchmarks/aft-search/engine-fixtures/plan-table.json"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search.rs"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/comparator.rs"),
                FenceRule::Exact(
                    "crates/aft/src/commands/semantic_search/evidence_descriptor.rs",
                ),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/generation_token.rs"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/mod.rs"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/plan_table.rs"),
                FenceRule::Exact("crates/aft/tests/engine_comparator_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_lint_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_plan_table_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/comparator/exact_tier_fixture.json",
                "benchmarks/aft-search/engine-fixtures/comparator/unit_3a_fixture.json",
                "benchmarks/aft-search/engine-fixtures/plan-table.json",
                "crates/aft/src/commands/semantic_search.rs",
                "crates/aft/src/commands/semantic_search/comparator.rs",
                "crates/aft/src/commands/semantic_search/evidence_descriptor.rs",
                "crates/aft/src/commands/semantic_search/generation_token.rs",
                "crates/aft/src/commands/semantic_search/mod.rs",
                "crates/aft/src/commands/semantic_search/plan_table.rs",
                "crates/aft/tests/engine_comparator_test.rs",
                "crates/aft/tests/engine_lint_test.rs",
                "crates/aft/tests/engine_plan_table_test.rs",
            ],
        },
        Slice {
            id: "A2-exact-lane-memo-lifecycle",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/exact/"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/exact_lane.rs"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/memo.rs"),
                FenceRule::Exact("crates/aft/src/search_index.rs"),
                FenceRule::Exact("crates/aft/tests/engine_content_binding_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_exact_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_fallback_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_restart_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/exact/alf_specimen.json",
                "benchmarks/aft-search/engine-fixtures/exact/bounded_exhaustion_c4.json",
                "benchmarks/aft-search/engine-fixtures/exact/census_episodes.json",
                "benchmarks/aft-search/engine-fixtures/exact/fallback_bounds.json",
                "benchmarks/aft-search/engine-fixtures/exact/uncapped_ready.json",
                "crates/aft/src/commands/semantic_search/exact_lane.rs",
                "crates/aft/src/commands/semantic_search/memo.rs",
                "crates/aft/src/search_index.rs",
                "crates/aft/tests/engine_content_binding_test.rs",
                "crates/aft/tests/engine_exact_test.rs",
                "crates/aft/tests/engine_fallback_test.rs",
                "crates/aft/tests/engine_restart_test.rs",
            ],
        },
        Slice {
            id: "A3-anchored-lane",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/anchored/"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/anchored_lane.rs"),
                FenceRule::Exact("crates/aft/tests/engine_anchored_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_i_phrase_lane_unreachable.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_ii_decoy_rejected_on_evidence.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_iii_single_run_qualification.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_iv_conjunction_negative.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_ix_intermediate_occurrence_totality.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_v_zero_runs_negative.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_vi_too_short_run_negative.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_vii_gap_tie_break.json",
                "benchmarks/aft-search/engine-fixtures/anchored/fixture_viii_repeated_runs_canonical_alignment.json",
                "crates/aft/src/commands/semantic_search/anchored_lane.rs",
                "crates/aft/tests/engine_anchored_test.rs",
            ],
        },
        Slice {
            id: "A4-lexical-depth-prefix-fidelity",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/lexical/"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/lexical_lane.rs"),
                FenceRule::Exact("crates/aft/tests/engine_lexical_depth_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/lexical/depth-prefix.json",
                "crates/aft/src/commands/semantic_search/lexical_lane.rs",
                "crates/aft/tests/engine_lexical_depth_test.rs",
            ],
        },
        Slice {
            id: "A5-blocks-tier-attribution-scoring",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/blocks/"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/blocks.rs"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/scoring.rs"),
                FenceRule::Exact("crates/aft/tests/engine_block_contract_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/blocks/tier-attribution.json",
                "crates/aft/src/commands/semantic_search/blocks.rs",
                "crates/aft/src/commands/semantic_search/scoring.rs",
                "crates/aft/tests/engine_block_contract_test.rs",
            ],
        },
        Slice {
            id: "A6-paging-offset-trailer",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/paging/"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/paging.rs"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/trailer.rs"),
                FenceRule::Exact("crates/aft/tests/engine_paging_contract_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_trailer_contract_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/paging/cases.json",
                "crates/aft/src/commands/semantic_search/paging.rs",
                "crates/aft/src/commands/semantic_search/trailer.rs",
                "crates/aft/tests/engine_paging_contract_test.rs",
                "crates/aft/tests/engine_trailer_contract_test.rs",
            ],
        },
        Slice {
            id: "A7-confidence-and-threshold-artifact",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/confidence/"),
                FenceRule::Exact("benchmarks/aft-search/engine-fixtures/confidence-threshold.json"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/confidence.rs"),
                FenceRule::Exact("crates/aft/tests/engine_confidence_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_confidence_threshold_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/confidence-threshold.json",
                "benchmarks/aft-search/engine-fixtures/confidence/cases.json",
                "crates/aft/src/commands/semantic_search/confidence.rs",
                "crates/aft/tests/engine_confidence_test.rs",
                "crates/aft/tests/engine_confidence_threshold_test.rs",
            ],
        },
        Slice {
            id: "A8-provenance-and-telemetry",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/provenance/"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/provenance.rs"),
                FenceRule::Exact("crates/aft/src/commands/semantic_search/telemetry.rs"),
                FenceRule::Exact("crates/aft/tests/engine_provenance_test.rs"),
                FenceRule::Exact("crates/aft/tests/engine_telemetry_test.rs"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/provenance/telemetry.json",
                "crates/aft/src/commands/semantic_search/provenance.rs",
                "crates/aft/src/commands/semantic_search/telemetry.rs",
                "crates/aft/tests/engine_provenance_test.rs",
                "crates/aft/tests/engine_telemetry_test.rs",
            ],
        },
        Slice {
            id: "A9-agent-surfaces-and-continuity",
            fence: vec![
                FenceRule::Prefix("benchmarks/aft-search/engine-fixtures/surface/"),
                FenceRule::Exact("crates/aft/src/subc_tool_schemas.json"),
                FenceRule::Exact("crates/aft/tests/engine_surface_contract_test.rs"),
                FenceRule::Exact("packages/opencode-plugin/src/__tests__/semantic.test.ts"),
                FenceRule::Exact("packages/opencode-plugin/src/tools/semantic.ts"),
                FenceRule::Exact("packages/pi-plugin/src/__tests__/semantic-renderers.test.ts"),
                FenceRule::Exact("packages/pi-plugin/src/__tests__/semantic.test.ts"),
                FenceRule::Exact("packages/pi-plugin/src/tools/semantic.ts"),
            ],
            landed_paths: vec![
                "benchmarks/aft-search/engine-fixtures/surface/cases.json",
                "crates/aft/src/subc_tool_schemas.json",
                "crates/aft/tests/engine_surface_contract_test.rs",
                "packages/opencode-plugin/src/__tests__/semantic.test.ts",
                "packages/opencode-plugin/src/tools/semantic.ts",
                "packages/pi-plugin/src/__tests__/semantic-renderers.test.ts",
                "packages/pi-plugin/src/__tests__/semantic.test.ts",
                "packages/pi-plugin/src/tools/semantic.ts",
            ],
        },
        Slice {
            id: "A10-fence-and-ownership-audit",
            fence: vec![FenceRule::Prefix("crates/aft/tests/engine_audit_")],
            landed_paths: vec![AUDIT_PATH],
        },
    ]
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn collect_files(root: &Path, relative_dir: &str, output: &mut BTreeSet<String>) {
    let directory = root.join(relative_dir);
    for entry in fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
    {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            let relative = path
                .strip_prefix(root)
                .expect("path below workspace")
                .to_string_lossy()
                .replace('\\', "/");
            collect_files(root, &relative, output);
        } else {
            output.insert(
                path.strip_prefix(root)
                    .expect("path below workspace")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

fn constraint_paths(root: &Path) -> BTreeSet<String> {
    let mut paths = BTreeSet::from([
        "crates/aft/src/commands/semantic_search.rs".to_string(),
        "crates/aft/src/search_index.rs".to_string(),
        "crates/aft/src/subc_tool_schemas.json".to_string(),
        "packages/opencode-plugin/src/__tests__/semantic.test.ts".to_string(),
        "packages/opencode-plugin/src/tools/semantic.ts".to_string(),
        "packages/pi-plugin/src/__tests__/semantic-renderers.test.ts".to_string(),
        "packages/pi-plugin/src/__tests__/semantic.test.ts".to_string(),
        "packages/pi-plugin/src/tools/semantic.ts".to_string(),
    ]);
    collect_files(root, "crates/aft/src/commands/semantic_search", &mut paths);
    collect_files(root, "benchmarks/aft-search/engine-fixtures", &mut paths);

    for entry in fs::read_dir(root.join("crates/aft/tests")).expect("read engine tests") {
        let path = entry.expect("engine test entry").path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.is_file() && name.starts_with("engine_") && name.ends_with(".rs") {
            paths.insert(format!("crates/aft/tests/{name}"));
        }
    }
    paths
}

fn is_b_owned(path: &str) -> bool {
    B_OWNED_EXACT.contains(&path) || path.starts_with(".github/workflows/")
}

fn validate_slice_map(root: &Path, slices: &[Slice]) -> Result<BTreeMap<String, String>, String> {
    let expected = constraint_paths(root);
    let mut owners = BTreeMap::new();
    let mut used_rules = BTreeSet::new();

    for path in &expected {
        let matching = slices
            .iter()
            .flat_map(|slice| {
                slice
                    .fence
                    .iter()
                    .enumerate()
                    .filter(move |(_, rule)| rule.matches(path))
                    .map(move |(index, rule)| (slice.id, index, rule.label()))
            })
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err(format!(
                "constraint path `{path}` has {} owners: {matching:?}",
                matching.len()
            ));
        }
        let (owner, rule_index, _) = matching[0];
        owners.insert(path.clone(), owner.to_string());
        used_rules.insert((owner, rule_index));
    }

    for slice in slices {
        for (index, rule) in slice.fence.iter().enumerate() {
            if !used_rules.contains(&(slice.id, index)) {
                return Err(format!(
                    "slice `{}` fence `{}` is outside the constraints union",
                    slice.id,
                    rule.label()
                ));
            }
        }
        for path in &slice.landed_paths {
            if !slice.fence.iter().any(|rule| rule.matches(path)) {
                return Err(format!(
                    "slice `{}` landed `{path}` outside its fence",
                    slice.id
                ));
            }
            if !expected.contains(*path) {
                return Err(format!(
                    "slice `{}` landed `{path}` outside the constraints union",
                    slice.id
                ));
            }
            if is_b_owned(path) {
                return Err(format!("slice `{}` touched B-owned `{path}`", slice.id));
            }
        }
    }

    let landed = slices
        .iter()
        .flat_map(|slice| slice.landed_paths.iter().copied())
        .collect::<BTreeSet<_>>();
    let expected_refs = expected.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if landed != expected_refs {
        return Err(format!(
            "slice landing inventory differs from constraints; missing={:?}, extra={:?}",
            expected_refs.difference(&landed).collect::<Vec<_>>(),
            landed.difference(&expected_refs).collect::<Vec<_>>()
        ));
    }

    Ok(owners)
}

#[test]
fn campaign_a_fences_are_disjoint_exhaustive_and_respected() {
    let root = workspace_root();
    let slices = slices();
    let owners = validate_slice_map(&root, &slices).expect("valid Campaign A slice ownership");

    assert_eq!(owners.len(), constraint_paths(&root).len());
    assert!(owners.keys().all(|path| !is_b_owned(path)));
    assert!(
        !root.join(RANKING_DIRECTORY).exists(),
        "{RANKING_DIRECTORY} must not exist"
    );
}

#[test]
fn semantic_stage_modules_have_one_owner_and_the_first_slice_owns_the_seam() {
    let root = workspace_root();
    let slices = slices();
    let owners = validate_slice_map(&root, &slices).expect("valid Campaign A slice ownership");

    let modules = constraint_paths(&root)
        .into_iter()
        .filter(|path| path.starts_with(SEMANTIC_MODULE_PREFIX))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        modules.len(),
        16,
        "every semantic lane/stage module is inventoried"
    );
    for module in modules {
        assert!(
            owners.contains_key(&module),
            "module `{module}` has no owner"
        );
    }

    for seam_path in SEAM_PATHS {
        assert_eq!(
            owners.get(*seam_path).map(String::as_str),
            Some(FIRST_SLICE)
        );
        let touching_slices = slices
            .iter()
            .filter(|slice| slice.landed_paths.contains(seam_path))
            .map(|slice| slice.id)
            .collect::<Vec<_>>();
        assert_eq!(
            touching_slices,
            [FIRST_SLICE],
            "`{seam_path}` was not landed once"
        );
    }
}

#[test]
fn campaign_b_deletion_case_leaves_the_engine_harness_self_contained() {
    let root = workspace_root();
    let slices = slices();
    validate_slice_map(&root, &slices).expect("valid Campaign A slice ownership");
    let projected_a_only_checkout = constraint_paths(&root);

    assert!(
        projected_a_only_checkout
            .iter()
            .all(|path| !is_b_owned(path)),
        "the A-only engine gate must remain complete after B-owned paths are deleted"
    );
    assert!(projected_a_only_checkout
        .iter()
        .any(|path| path.starts_with(ENGINE_FIXTURE_PREFIX)));
    assert!(projected_a_only_checkout
        .iter()
        .any(|path| path.starts_with(ENGINE_TEST_PREFIX)));

    for path in projected_a_only_checkout
        .iter()
        .filter(|path| path.starts_with(ENGINE_FIXTURE_PREFIX) && path.ends_with(".json"))
    {
        let text = fs::read_to_string(root.join(path))
            .unwrap_or_else(|error| panic!("read engine fixture `{path}`: {error}"));
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|error| panic!("parse engine fixture `{path}`: {error}"));
    }

    for path in projected_a_only_checkout
        .iter()
        .filter(|path| *path != AUDIT_PATH && root.join(path).is_file())
    {
        let text = fs::read_to_string(root.join(path))
            .unwrap_or_else(|error| panic!("read Campaign A input `{path}`: {error}"));
        for forbidden in B_OWNED_EXACT {
            assert!(
                !text.contains(forbidden),
                "Campaign A input `{path}` depends on deleted B-owned path `{forbidden}`"
            );
        }
        assert!(
            !text.contains("search-quality-gate"),
            "Campaign A input `{path}` waits on the deleted B-owned workflow"
        );
    }
}

#[test]
fn audit_rejects_an_out_of_fence_landing() {
    let root = workspace_root();
    let mut slices = slices();
    slices[0]
        .landed_paths
        .push("crates/aft/src/semantic_index.rs");
    let error = validate_slice_map(&root, &slices).expect_err("out-of-fence landing must fail");
    assert!(error.contains("outside its fence"));
}

#[test]
fn audit_rejects_overlapping_fences() {
    let root = workspace_root();
    let mut slices = slices();
    slices.last_mut().unwrap().fence.push(FenceRule::Exact(
        "crates/aft/src/commands/semantic_search/comparator.rs",
    ));
    let error = validate_slice_map(&root, &slices).expect_err("overlapping fence must fail");
    assert!(error.contains("2 owners"));
}
