use std::path::Path;

use aft::commands::semantic_search::plan_table::{
    verify_pinned_plan_table_at_startup, LanePlanEntry, PlanTable, SearchLaneKind, SearchShape,
};

#[test]
fn test_running_table_matches_pinned_json() {
    verify_pinned_plan_table_at_startup().expect("running table must match pinned plan-table.json");

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = if manifest_dir
        .join("../../benchmarks/aft-search/engine-fixtures/plan-table.json")
        .exists()
    {
        manifest_dir.join("../../benchmarks/aft-search/engine-fixtures/plan-table.json")
    } else {
        Path::new("benchmarks/aft-search/engine-fixtures/plan-table.json").to_path_buf()
    };
    let table_from_file = PlanTable::from_file(&fixture_path)
        .expect("must successfully load benchmarks/aft-search/engine-fixtures/plan-table.json");
    let running = PlanTable::running_table();
    running
        .verify_against(&table_from_file)
        .expect("file-loaded pinned table must match running table");

    // The product embeds its own copy of the pinned table (a product build
    // cannot reach outside crates/). The benchmark copy is the one the
    // fixtures and the gate read; the two must stay byte-identical.
    let benchmark_bytes =
        std::fs::read(&fixture_path).expect("benchmark plan-table.json must be readable");
    assert_eq!(
        aft::commands::semantic_search::plan_table::PINNED_PLAN_TABLE_JSON.as_bytes(),
        benchmark_bytes.as_slice(),
        "crates/aft/assets/search-plan-table.pinned.json must be byte-identical to benchmarks/aft-search/engine-fixtures/plan-table.json"
    );
}

#[test]
fn test_cross_product_totality_63_entries() {
    let running = PlanTable::running_table();
    assert_eq!(running.entries.len(), 7, "must have 7 shapes");
    for shape in SearchShape::ALL {
        let lanes = running
            .entries
            .get(shape.as_str())
            .unwrap_or_else(|| panic!("missing shape: {}", shape.as_str()));
        assert_eq!(lanes.len(), 9, "each shape must have exactly 9 lanes");
        for lane in SearchLaneKind::ALL {
            assert!(
                lanes.contains_key(lane.as_str()),
                "shape {} missing lane {}",
                shape.as_str(),
                lane.as_str()
            );
        }
    }
}

#[test]
fn test_missing_pair_is_error_naming_triple() {
    let running = PlanTable::running_table();
    let mut pinned = PlanTable::running_table();
    pinned
        .entries
        .get_mut("identifier")
        .unwrap()
        .remove("lexical");

    let err = running.verify_against(&pinned).unwrap_err();
    assert_eq!(err.shape, "identifier");
    assert_eq!(err.lane, "lexical");
    assert_eq!(err.field, "pair");
    let err_msg = err.to_string();
    assert!(err_msg.contains("(identifier, lexical, pair)"));
}

#[test]
fn test_extra_pair_is_error_naming_triple() {
    let running = PlanTable::running_table();
    let mut pinned = PlanTable::running_table();
    pinned.entries.get_mut("regex").unwrap().insert(
        "custom_lane".to_string(),
        LanePlanEntry {
            weight: Some(0.5),
            rrf_constant: Some(60.0),
            plan_order_index: 3,
        },
    );

    let err = running.verify_against(&pinned).unwrap_err();
    assert_eq!(err.shape, "regex");
    assert_eq!(err.lane, "custom_lane");
    assert_eq!(err.field, "pair");
    let err_msg = err.to_string();
    assert!(err_msg.contains("(regex, custom_lane, pair)"));
}

#[test]
fn test_mutation_red_one_shape_weight_change() {
    let running = PlanTable::running_table();
    let mut pinned = PlanTable::running_table();
    pinned
        .entries
        .get_mut("path")
        .unwrap()
        .get_mut("lexical")
        .unwrap()
        .weight = Some(0.85); // changed from 0.9

    let err = running.verify_against(&pinned).unwrap_err();
    assert_eq!(err.shape, "path");
    assert_eq!(err.lane, "lexical");
    assert_eq!(err.field, "weight");
    let err_msg = err.to_string();
    assert!(err_msg.contains("(path, lexical, weight)"));
}

#[test]
fn test_mutation_red_all_shape_rrf_change() {
    let running = PlanTable::running_table();
    let mut pinned = PlanTable::running_table();
    for (_, lanes) in pinned.entries.iter_mut() {
        for (lane_name, entry) in lanes.iter_mut() {
            if lane_name != "exact" {
                entry.rrf_constant = Some(50.0); // changed from 60.0
            }
        }
    }

    let err = running.verify_against(&pinned).unwrap_err();
    assert_eq!(err.field, "rrf_constant");
    let err_msg = err.to_string();
    assert!(err_msg.contains("rrf_constant"));
}

#[test]
fn test_mutation_red_exact_lane_zero_weight_instead_of_null() {
    let running = PlanTable::running_table();
    let mut pinned = PlanTable::running_table();
    pinned
        .entries
        .get_mut("nl")
        .unwrap()
        .get_mut("exact")
        .unwrap()
        .weight = Some(0.0); // zero weight instead of null

    let err = running.verify_against(&pinned).unwrap_err();
    assert_eq!(err.shape, "nl");
    assert_eq!(err.lane, "exact");
    assert_eq!(err.field, "weight");
    let err_msg = err.to_string();
    assert!(err_msg.contains("(nl, exact, weight)"));
}

#[test]
fn test_exact_lane_non_null_rrf_constant_is_error() {
    let running = PlanTable::running_table();
    let mut pinned = PlanTable::running_table();
    pinned
        .entries
        .get_mut("code_literal")
        .unwrap()
        .get_mut("exact")
        .unwrap()
        .rrf_constant = Some(60.0); // non-null rrf_constant on exact lane

    let err = running.verify_against(&pinned).unwrap_err();
    assert_eq!(err.shape, "code_literal");
    assert_eq!(err.lane, "exact");
    assert_eq!(err.field, "rrf_constant");
    let err_msg = err.to_string();
    assert!(err_msg.contains("(code_literal, exact, rrf_constant)"));
}

#[test]
fn test_changed_plan_order_index_is_error() {
    let running = PlanTable::running_table();
    let mut pinned = PlanTable::running_table();
    pinned
        .entries
        .get_mut("identifier")
        .unwrap()
        .get_mut("lexical")
        .unwrap()
        .plan_order_index = 2; // changed from 1 to 2

    let err = running.verify_against(&pinned).unwrap_err();
    assert_eq!(err.shape, "identifier");
    assert_eq!(err.lane, "lexical");
    assert_eq!(err.field, "plan_order_index");
    let err_msg = err.to_string();
    assert!(err_msg.contains("(identifier, lexical, plan_order_index)"));
}
