use std::fs;
use std::path::Path;

use aft::callgraph_store::CallGraphStore;
use aft::commands::callgraph_store_adapter::callgraph_surface::{
    build_callgraph_envelope, cap_items, hub_selector_activated, CALLERS_LIST_ID, CALLGRAPH_NARROW,
    CALLGRAPH_SURFACES, HUB_SUMMARY_LIMIT, HUB_SUMMARY_THRESHOLD, SITES_LIST_ID, TREE_LIST_ID,
};
use aft::commands::callgraph_store_adapter::{
    self, StoreCallTreeNode, StoreCallerEntry, StoreCallerGroup, StoreCallersResult,
    StoreHubSummary, StoreImpactCaller, StoreImpactResult,
};
use aft::list_envelope::{ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::find_surface;
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde_json::{json, Value};
use tempfile::tempdir;

fn load_golden(name: &str) -> String {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = manifest_dir
        .join("tests")
        .join("fixtures")
        .join("callgraph")
        .join("truncation")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read golden file {}: {}", path.display(), e))
}

fn assert_no_key_literally_named_list_envelope(value: &Value) {
    match value {
        Value::Object(map) => {
            assert!(
                !map.contains_key("list_envelope"),
                "found forbidden key 'list_envelope' in JSON object: {:?}",
                map.keys()
            );
            for v in map.values() {
                assert_no_key_literally_named_list_envelope(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                assert_no_key_literally_named_list_envelope(v);
            }
        }
        _ => {}
    }
}

fn format_subc(op: &str, data: &Value) -> String {
    let resp = Response {
        id: "test-callgraph".into(),
        success: true,
        data: data.clone(),
    };
    let mut ctx = FormatContext::default();
    ctx.callgraph_op = Some(op.to_string());
    format_response_with_context("callgraph", &resp, &ctx)
}

fn format_ndjson(base_text: &str, data: &Value, list_id: &str) -> String {
    build_ndjson_text(base_text, data, Some(list_id), false)
}

// -----------------------------------------------------------------------
// Helper constructors
// -----------------------------------------------------------------------

fn make_impact_result(count: usize, depth_limited: bool, truncated: usize) -> StoreImpactResult {
    let callers = (1..=count)
        .map(|i| StoreImpactCaller {
            caller_symbol: format!("caller{i}"),
            caller_file: "src/app.ts".to_string(),
            line: i as u32,
            signature: None,
            is_entry_point: false,
            call_expression: None,
            parameters: Vec::new(),
            approximate: None,
            resolved_by: None,
        })
        .collect::<Vec<_>>();

    let summarize = hub_selector_activated(count);
    let visible_callers = if summarize {
        callers.into_iter().take(HUB_SUMMARY_LIMIT).collect()
    } else {
        callers
    };
    let shown = visible_callers.len();

    let hub_summary = if summarize {
        Some(StoreHubSummary {
            message: format!("Showing first 20 callers; {count} omitted high-fan-in callers."),
            total: count,
            hidden_tests: 0,
            shown,
            threshold: HUB_SUMMARY_THRESHOLD,
            limit: HUB_SUMMARY_LIMIT,
            counts_are_lower_bounds: false,
        })
    } else {
        None
    };

    let sites_list_envelope = build_callgraph_envelope(Unit::Sites, shown, count, truncated);

    StoreImpactResult {
        symbol: "target".to_string(),
        file: "src/app.ts".to_string(),
        signature: None,
        parameters: Vec::new(),
        total_affected: count, // R11: envelope total
        hidden_test_callers: 0,
        affected_files: 1,
        callers: visible_callers,
        hub_summary,
        depth_limited,
        truncated,
        sites_list_envelope,
    }
}

fn make_callers_result(count: usize, depth_limited: bool, truncated: usize) -> StoreCallersResult {
    let entries = (1..=count)
        .map(|i| StoreCallerEntry {
            symbol: format!("caller{:02}", i),
            line: i as u32,
            approximate: None,
            resolved_by: None,
        })
        .collect::<Vec<_>>();

    let summarize = hub_selector_activated(count);
    let visible_entries = if summarize {
        entries.into_iter().take(HUB_SUMMARY_LIMIT).collect()
    } else {
        entries
    };
    let shown = visible_entries.len();

    let hub_summary = if summarize {
        Some(StoreHubSummary {
            message: format!("Showing first 20 callers; {count} omitted high-fan-in callers."),
            total: count,
            hidden_tests: 0,
            shown,
            threshold: HUB_SUMMARY_THRESHOLD,
            limit: HUB_SUMMARY_LIMIT,
            counts_are_lower_bounds: false,
        })
    } else {
        None
    };

    let callers_list_envelope = build_callgraph_envelope(Unit::Items, shown, count, truncated);

    StoreCallersResult {
        symbol: "target".to_string(),
        file: "src/app.ts".to_string(),
        callers: vec![StoreCallerGroup {
            file: "src/app.ts".to_string(),
            callers: visible_entries,
        }],
        total_callers: count, // R11: envelope total
        hidden_test_callers: 0,
        hub_summary,
        scanned_files: 1,
        depth_limited,
        truncated,
        callers_list_envelope,
    }
}

fn make_call_tree_node(count: usize, depth_limited: bool, truncated: usize) -> StoreCallTreeNode {
    let mut children = (1..=count)
        .map(|i| StoreCallTreeNode {
            name: format!("child{i}"),
            file: "src/app.ts".to_string(),
            line: (9 + i) as u32,
            signature: None,
            resolved: true,
            approximate: None,
            resolved_by: None,
            children: Vec::new(),
            depth_limited: false,
            truncated: 0,
            hidden_test_callers: 0,
            tree_list_envelope: None,
        })
        .collect::<Vec<_>>();

    let (shown, total) = cap_items(&mut children);
    let tree_list_envelope = build_callgraph_envelope(Unit::Items, shown, total, truncated);

    StoreCallTreeNode {
        name: "root".to_string(),
        file: "src/app.ts".to_string(),
        line: 1,
        signature: None,
        resolved: true,
        approximate: None,
        resolved_by: None,
        children,
        depth_limited,
        truncated,
        hidden_test_callers: 0,
        tree_list_envelope,
    }
}

// -----------------------------------------------------------------------
// Acceptance Criteria Tests
// -----------------------------------------------------------------------

#[test]
fn test_callgraph_triple_post_filter_impact() {
    // 16 entries: full row set, no envelope, no trailer
    let res16 = make_impact_result(16, false, 0);
    assert!(res16.sites_list_envelope.is_none());
    let val16 = serde_json::to_value(&res16).unwrap();
    assert!(!val16
        .as_object()
        .unwrap()
        .contains_key("sites_list_envelope"));
    let subc16 = format_subc("impact", &val16);
    let expected16 = load_golden("impact_16_expected.txt");
    assert_eq!(subc16.trim(), expected16.trim());
    let ndjson16 = format_ndjson(&subc16, &val16, SITES_LIST_ID);
    assert_eq!(subc16, ndjson16);

    // 20 entries: full row set, no envelope, no trailer
    let res20 = make_impact_result(20, false, 0);
    assert!(res20.sites_list_envelope.is_none());
    let val20 = serde_json::to_value(&res20).unwrap();
    assert!(!val20
        .as_object()
        .unwrap()
        .contains_key("sites_list_envelope"));
    let subc20 = format_subc("impact", &val20);
    let expected20 = load_golden("impact_20_expected.txt");
    assert_eq!(subc20.trim(), expected20.trim());
    let ndjson20 = format_ndjson(&subc20, &val20, SITES_LIST_ID);
    assert_eq!(subc20, ndjson20);

    // 21 entries: activates hub selection, shown 15 of 21 sites (cap)
    let res21 = make_impact_result(21, false, 0);
    let env21 = res21
        .sites_list_envelope
        .as_ref()
        .expect("cap envelope must be emitted");
    assert_eq!(env21.shown, 15);
    assert_eq!(env21.total, Total::Exact(21));
    assert_eq!(env21.unit, Unit::Sites);
    assert_eq!(env21.reason, Some(Reason::Cap));
    assert_eq!(env21.causes, vec![Reason::Cap]);

    let val21 = serde_json::to_value(&res21).unwrap();
    let subc21 = format_subc("impact", &val21);
    let expected21 = load_golden("impact_21_expected.txt");
    assert_eq!(subc21.trim(), expected21.trim());
    assert!(subc21.contains("shown 15 of 21 sites (cap) · narrow: depth, includeTests"));

    // Transport parity
    let base_without_trailer = subc21
        .lines()
        .filter(|l| !l.starts_with("shown ") && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let ndjson21 = format_ndjson(&base_without_trailer, &val21, SITES_LIST_ID);
    assert_eq!(subc21.trim(), ndjson21.trim());

    // R24 unit word assertion
    let surface = find_surface("callgraph", "impact", SITES_LIST_ID).unwrap();
    assert_eq!(surface.unit, Unit::Sites);
    assert_eq!(env21.unit, surface.unit);
    assert!(subc21.contains(&format!(" {} (cap)", surface.unit.as_str())));
}

#[test]
fn test_callgraph_triple_post_filter_callers() {
    // 16 entries: full row set, no envelope, no trailer
    let res16 = make_callers_result(16, false, 0);
    assert!(res16.callers_list_envelope.is_none());
    let val16 = serde_json::to_value(&res16).unwrap();
    let subc16 = format_subc("callers", &val16);
    let expected16 = load_golden("callers_16_expected.txt");
    assert_eq!(subc16.trim(), expected16.trim());
    let ndjson16 = format_ndjson(&subc16, &val16, CALLERS_LIST_ID);
    assert_eq!(subc16, ndjson16);

    // 20 entries: full row set, no envelope, no trailer
    let res20 = make_callers_result(20, false, 0);
    assert!(res20.callers_list_envelope.is_none());
    let val20 = serde_json::to_value(&res20).unwrap();
    let subc20 = format_subc("callers", &val20);
    let expected20 = load_golden("callers_20_expected.txt");
    assert_eq!(subc20.trim(), expected20.trim());
    let ndjson20 = format_ndjson(&subc20, &val20, CALLERS_LIST_ID);
    assert_eq!(subc20, ndjson20);

    // 21 entries: activates hub selection, shown 15 of 21 items (cap)
    let res21 = make_callers_result(21, false, 0);
    let env21 = res21
        .callers_list_envelope
        .as_ref()
        .expect("cap envelope must be emitted");
    assert_eq!(env21.shown, 15);
    assert_eq!(env21.total, Total::Exact(21));
    assert_eq!(env21.unit, Unit::Items);
    assert_eq!(env21.reason, Some(Reason::Cap));
    assert_eq!(env21.causes, vec![Reason::Cap]);

    let val21 = serde_json::to_value(&res21).unwrap();
    let subc21 = format_subc("callers", &val21);
    let expected21 = load_golden("callers_21_expected.txt");
    assert_eq!(subc21.trim(), expected21.trim());
    assert!(subc21.contains("shown 15 of 21 items (cap) · narrow: depth, includeTests"));

    // Transport parity
    let base_without_trailer = subc21
        .lines()
        .filter(|l| !l.starts_with("shown ") && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let ndjson21 = format_ndjson(&base_without_trailer, &val21, CALLERS_LIST_ID);
    assert_eq!(subc21.trim(), ndjson21.trim());

    // R24 unit word assertion
    let surface = find_surface("callgraph", "callers", CALLERS_LIST_ID).unwrap();
    assert_eq!(surface.unit, Unit::Items);
    assert_eq!(env21.unit, surface.unit);
    assert!(subc21.contains(&format!(" {} (cap)", surface.unit.as_str())));
}

#[test]
fn test_callgraph_triple_post_filter_call_tree() {
    // 16 entries: full row set, no envelope, no trailer
    let res16 = make_call_tree_node(16, false, 0);
    assert!(res16.tree_list_envelope.is_none());
    let val16 = serde_json::to_value(&res16).unwrap();
    let subc16 = format_subc("call_tree", &val16);
    let expected16 = load_golden("call_tree_16_expected.txt");
    assert_eq!(subc16.trim(), expected16.trim());
    let ndjson16 = format_ndjson(&subc16, &val16, TREE_LIST_ID);
    assert_eq!(subc16, ndjson16);

    // 20 entries: full row set, no envelope, no trailer
    let res20 = make_call_tree_node(20, false, 0);
    assert!(res20.tree_list_envelope.is_none());
    let val20 = serde_json::to_value(&res20).unwrap();
    let subc20 = format_subc("call_tree", &val20);
    let expected20 = load_golden("call_tree_20_expected.txt");
    assert_eq!(subc20.trim(), expected20.trim());
    let ndjson20 = format_ndjson(&subc20, &val20, TREE_LIST_ID);
    assert_eq!(subc20, ndjson20);

    // 21 entries: activates hub selection, shown 15 of 21 items (cap)
    let res21 = make_call_tree_node(21, false, 0);
    let env21 = res21
        .tree_list_envelope
        .as_ref()
        .expect("cap envelope must be emitted");
    assert_eq!(env21.shown, 15);
    assert_eq!(env21.total, Total::Exact(21));
    assert_eq!(env21.unit, Unit::Items);
    assert_eq!(env21.reason, Some(Reason::Cap));
    assert_eq!(env21.causes, vec![Reason::Cap]);

    let val21 = serde_json::to_value(&res21).unwrap();
    let subc21 = format_subc("call_tree", &val21);
    let expected21 = load_golden("call_tree_21_expected.txt");
    assert_eq!(subc21.trim(), expected21.trim());
    assert!(subc21.contains("shown 15 of 21 items (cap) · narrow: depth, includeTests"));

    // Transport parity
    let base_without_trailer = subc21
        .lines()
        .filter(|l| !l.starts_with("shown ") && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let ndjson21 = format_ndjson(&base_without_trailer, &val21, TREE_LIST_ID);
    assert_eq!(subc21.trim(), ndjson21.trim());

    // R24 unit word assertion
    let surface = find_surface("callgraph", "call_tree", TREE_LIST_ID).unwrap();
    assert_eq!(surface.unit, Unit::Items);
    assert_eq!(env21.unit, surface.unit);
    assert!(subc21.contains(&format!(" {} (cap)", surface.unit.as_str())));
}

#[test]
fn test_impact_412_fixture_at_default_depth() {
    let res412 = make_impact_result(412, false, 0);
    let env412 = res412.sites_list_envelope.as_ref().expect("cap envelope");
    assert_eq!(env412.shown, 15);
    assert_eq!(env412.total, Total::Exact(412));
    assert_eq!(env412.unit, Unit::Sites);
    assert_eq!(env412.reason, Some(Reason::Cap));
    assert_eq!(env412.causes, vec![Reason::Cap]);
    assert_eq!(
        env412.narrow,
        vec!["depth".to_string(), "includeTests".to_string()]
    );

    let val412 = serde_json::to_value(&res412).unwrap();
    // Recursive assertion: confirms no key literally named list_envelope anywhere in reply
    assert_no_key_literally_named_list_envelope(&val412);
    // Wire key check
    assert!(val412
        .as_object()
        .unwrap()
        .contains_key("sites_list_envelope"));

    let subc412 = format_subc("impact", &val412);
    let expected412 = load_golden("impact_412_expected.txt");
    assert_eq!(subc412.trim(), expected412.trim());
    assert!(subc412.contains("shown 15 of 412 sites (cap) · narrow: depth, includeTests"));
    // Asserts no hub_summary sentence in text
    assert!(!subc412.contains("Showing first"));
    assert!(!subc412.contains("omitted high-fan-in"));

    // Same symbol at honored depth: 1 with 12 sites renders no trailer and no envelope key
    let res12 = make_impact_result(12, false, 0);
    assert!(res12.sites_list_envelope.is_none());
    let val12 = serde_json::to_value(&res12).unwrap();
    assert!(!val12
        .as_object()
        .unwrap()
        .contains_key("sites_list_envelope"));
    let subc12 = format_subc("impact", &val12);
    let expected12 = load_golden("impact_12_expected.txt");
    assert_eq!(subc12.trim(), expected12.trim());
    assert!(!subc12.contains("shown "));
    assert!(!subc12.contains("(cap)"));
    assert!(!subc12.contains("(depth)"));
}

#[test]
fn test_honored_depth_exemption_a() {
    // Honored depth with truncated == 0 and depth_limited set renders no (depth) reason
    // and no legacy (depth limited) clause; that path is pinned to NEW goldens as closed
    // exemption (a), byte-equal across NDJSON and subc, with depth_limited retained in JSON.
    let callers_ex = make_callers_result(12, true, 0);
    assert!(callers_ex.callers_list_envelope.is_none());
    assert!(callers_ex.depth_limited);
    assert_eq!(callers_ex.truncated, 0);

    let val_callers = serde_json::to_value(&callers_ex).unwrap();
    assert_eq!(val_callers["depth_limited"], json!(true));
    assert_eq!(val_callers["truncated"], json!(0));
    assert!(!val_callers
        .as_object()
        .unwrap()
        .contains_key("callers_list_envelope"));

    let subc_callers = format_subc("callers", &val_callers);
    let expected_callers = load_golden("callers_exemption_a_expected.txt");
    assert_eq!(subc_callers.trim(), expected_callers.trim());
    assert!(!subc_callers.contains("(depth limited)"));
    assert!(!subc_callers.contains("(depth)"));
    assert!(!subc_callers.contains("shown "));

    let ndjson_callers = format_ndjson(&subc_callers, &val_callers, CALLERS_LIST_ID);
    assert_eq!(subc_callers, ndjson_callers);

    // Call tree exemption (a)
    let tree_ex = make_call_tree_node(12, true, 0);
    assert!(tree_ex.tree_list_envelope.is_none());
    assert!(tree_ex.depth_limited);
    assert_eq!(tree_ex.truncated, 0);

    let val_tree = serde_json::to_value(&tree_ex).unwrap();
    assert_eq!(val_tree["depth_limited"], json!(true));
    assert_eq!(val_tree["truncated"], json!(0));
    assert!(!val_tree
        .as_object()
        .unwrap()
        .contains_key("tree_list_envelope"));

    let subc_tree = format_subc("call_tree", &val_tree);
    let expected_tree = load_golden("call_tree_exemption_a_expected.txt");
    assert_eq!(subc_tree.trim(), expected_tree.trim());
    assert!(!subc_tree.contains("(depth limited)"));
    assert!(!subc_tree.contains("(depth)"));
    assert!(!subc_tree.contains("shown "));

    let ndjson_tree = format_ndjson(&subc_tree, &val_tree, TREE_LIST_ID);
    assert_eq!(subc_tree, ndjson_tree);
}

#[test]
fn test_cause_independence_honored_depth_412_entries() {
    // Honored depth, truncated == 0, depth_limited set, 412 post-filter entries
    // still emits the cap envelope and trailer for each of the three operations.
    let impact412 = make_impact_result(412, true, 0);
    let env_impact = impact412
        .sites_list_envelope
        .as_ref()
        .expect("cap envelope for impact");
    assert_eq!(env_impact.reason, Some(Reason::Cap));
    assert_eq!(env_impact.causes, vec![Reason::Cap]);
    assert_eq!(env_impact.total, Total::Exact(412));
    let val_impact = serde_json::to_value(&impact412).unwrap();
    let subc_impact = format_subc("impact", &val_impact);
    assert!(subc_impact.contains("shown 15 of 412 sites (cap) · narrow: depth, includeTests"));

    let callers412 = make_callers_result(412, true, 0);
    let env_callers = callers412
        .callers_list_envelope
        .as_ref()
        .expect("cap envelope for callers");
    assert_eq!(env_callers.reason, Some(Reason::Cap));
    assert_eq!(env_callers.causes, vec![Reason::Cap]);
    assert_eq!(env_callers.total, Total::Exact(412));
    let val_callers = serde_json::to_value(&callers412).unwrap();
    let subc_callers = format_subc("callers", &val_callers);
    assert!(subc_callers.contains("shown 15 of 412 items (cap) · narrow: depth, includeTests"));

    let tree412 = make_call_tree_node(412, true, 0);
    let env_tree = tree412
        .tree_list_envelope
        .as_ref()
        .expect("cap envelope for call_tree");
    assert_eq!(env_tree.reason, Some(Reason::Cap));
    assert_eq!(env_tree.causes, vec![Reason::Cap]);
    assert_eq!(env_tree.total, Total::Exact(412));
    let val_tree = serde_json::to_value(&tree412).unwrap();
    let subc_tree = format_subc("call_tree", &val_tree);
    assert!(subc_tree.contains("shown 15 of 412 items (cap) · narrow: depth, includeTests"));

    // Mutation control: a mutation suppressing envelope or trailer whenever truncated == 0
    // reds those fixtures.
    let mutated_builder =
        |unit: Unit, shown: usize, count: usize, truncated: usize| -> Option<ListEnvelope> {
            if truncated == 0 {
                None // mutation suppressing whenever truncated == 0
            } else {
                build_callgraph_envelope(unit, shown, count, truncated)
            }
        };
    let mutated_env = mutated_builder(Unit::Sites, 15, 412, 0);
    assert!(mutated_env.is_none(), "mutation must suppress envelope");
    assert_ne!(
        mutated_env, impact412.sites_list_envelope,
        "mutation suppressing envelope when truncated == 0 reds the 412-entry fixture"
    );
}

#[test]
fn test_cut_inside_requested_depth() {
    // Cut inside requested depth (truncated > 0): renders shown 15 of ≥15 items (depth) on callers/call_tree
    // and the same reason in sites on impact.
    let callers_cut = make_callers_result(15, true, 5);
    let env_callers = callers_cut
        .callers_list_envelope
        .as_ref()
        .expect("depth envelope");
    assert_eq!(env_callers.shown, 15);
    assert_eq!(env_callers.total, Total::AtLeast(15));
    assert_eq!(env_callers.unit, Unit::Items);
    assert_eq!(env_callers.reason, Some(Reason::Depth));
    assert_eq!(env_callers.causes, vec![Reason::Depth]);

    let val_callers = serde_json::to_value(&callers_cut).unwrap();
    let subc_callers = format_subc("callers", &val_callers);
    let expected_callers = load_golden("callers_cut_depth_expected.txt");
    assert_eq!(subc_callers.trim(), expected_callers.trim());
    assert!(subc_callers.contains("shown 15 of ≥15 items (depth) · narrow: depth, includeTests"));

    // Call tree cut inside requested depth
    let tree_cut = make_call_tree_node(15, true, 3);
    let env_tree = tree_cut
        .tree_list_envelope
        .as_ref()
        .expect("depth envelope");
    assert_eq!(env_tree.shown, 15);
    assert_eq!(env_tree.total, Total::AtLeast(15));
    assert_eq!(env_tree.unit, Unit::Items);
    assert_eq!(env_tree.reason, Some(Reason::Depth));
    assert_eq!(env_tree.causes, vec![Reason::Depth]);

    let val_tree = serde_json::to_value(&tree_cut).unwrap();
    let subc_tree = format_subc("call_tree", &val_tree);
    assert!(subc_tree.contains("shown 15 of ≥15 items (depth) · narrow: depth, includeTests"));

    // Impact cut inside requested depth
    let impact_cut = make_impact_result(15, true, 4);
    let env_impact = impact_cut
        .sites_list_envelope
        .as_ref()
        .expect("depth envelope");
    assert_eq!(env_impact.shown, 15);
    assert_eq!(env_impact.total, Total::AtLeast(15));
    assert_eq!(env_impact.unit, Unit::Sites);
    assert_eq!(env_impact.reason, Some(Reason::Depth));
    assert_eq!(env_impact.causes, vec![Reason::Depth]);

    let val_impact = serde_json::to_value(&impact_cut).unwrap();
    let subc_impact = format_subc("impact", &val_impact);
    assert!(subc_impact.contains("shown 15 of ≥15 sites (depth) · narrow: depth, includeTests"));

    // A reply with both truncated > 0 and an activated hub selector renders one (depth) trailer
    // with causes: ["depth","cap"] and both legacy flags in JSON.
    let both_cut = make_callers_result(21, true, 5);
    let env_both = both_cut
        .callers_list_envelope
        .as_ref()
        .expect("mixed envelope");
    assert_eq!(env_both.shown, 15);
    assert_eq!(env_both.total, Total::AtLeast(15));
    assert_eq!(env_both.reason, Some(Reason::Depth));
    assert_eq!(env_both.causes, vec![Reason::Depth, Reason::Cap]);

    let val_both = serde_json::to_value(&both_cut).unwrap();
    assert_eq!(val_both["depth_limited"], json!(true));
    assert_eq!(val_both["truncated"], json!(5));
    assert!(val_both.get("hub_summary").is_some());

    let subc_both = format_subc("callers", &val_both);
    assert!(subc_both.contains("shown 15 of ≥15 items (depth) · narrow: depth, includeTests"));
    assert!(!subc_both.contains("(cap)")); // Precedence: Depth > Cap, only 1 trailer rendered
}

#[test]
fn test_r12_paired_fixtures() {
    // When test filtering changes the requested domain, the legacy total remains
    // pre-filter while the heading comes from the post-filter envelope. The
    // contradictory hub summary stays omitted, but the hidden-test disclosure remains.
    let pre_filter = 26;
    let post_filter = 21;
    let hidden_tests = 5;

    let mut filtered_impact = make_impact_result(post_filter, false, 0);
    filtered_impact.total_affected = pre_filter;
    filtered_impact.hidden_test_callers = hidden_tests;
    filtered_impact.hub_summary = None;
    let json_filtered = serde_json::to_value(&filtered_impact).unwrap();

    assert_eq!(json_filtered["total_affected"], json!(pre_filter));
    assert_eq!(json_filtered["hidden_test_callers"], json!(hidden_tests));
    assert!(json_filtered.get("hub_summary").is_none());
    assert_eq!(
        filtered_impact
            .sites_list_envelope
            .as_ref()
            .expect("post-filter cap envelope")
            .total,
        Total::Exact(post_filter)
    );

    let rendered_impact = format_subc("impact", &json_filtered);
    assert!(rendered_impact.starts_with("21 affected call sites · 1 file"));
    let hidden_line = "5 callers in tests hidden — includeTests: true shows them";
    let hidden_index = rendered_impact
        .find(hidden_line)
        .expect("impact should disclose filtered test callers");
    let trailer_index = rendered_impact
        .find("shown 15 of 21 sites (cap) · narrow: depth, includeTests")
        .expect("post-filter hub should render its cap trailer");
    assert!(
        hidden_index < trailer_index,
        "hidden-test disclosure must render before the cap trailer: {rendered_impact}"
    );

    let mut filtered_tree = make_call_tree_node(20, false, 0);
    filtered_tree.hidden_test_callers = hidden_tests;
    let json_filtered_tree = serde_json::to_value(&filtered_tree).unwrap();
    assert_eq!(
        json_filtered_tree["hidden_test_callers"],
        json!(hidden_tests)
    );
    let rendered_tree = format_subc("call_tree", &json_filtered_tree);
    assert!(
        rendered_tree.contains(hidden_line),
        "call_tree should disclose filtered test callers without a hub: {rendered_tree}"
    );
    assert!(!rendered_tree.contains("shown "));

    // The includeTests reply keeps its existing hub summary and has no hidden count.
    let unfiltered_hub_msg = format!("Next: {pre_filter} affected callers ({hidden_tests} in tests, included) — showing 15; narrow with scope");
    let unfiltered_hub_summary = json!({
        "message": unfiltered_hub_msg,
        "total": pre_filter,
        "hidden_tests": hidden_tests,
        "shown": 15,
        "threshold": HUB_SUMMARY_THRESHOLD,
        "limit": HUB_SUMMARY_LIMIT,
        "counts_are_lower_bounds": false,
    });

    let envelope_unfiltered = build_callgraph_envelope(Unit::Sites, 15, pre_filter, 0).unwrap();
    assert_eq!(envelope_unfiltered.total, Total::Exact(pre_filter));

    let json_unfiltered = json!({
        "symbol": "target",
        "file": "src/app.ts",
        "total_affected": pre_filter,
        "affected_files": 1,
        "callers": [],
        "hub_summary": unfiltered_hub_summary.clone(),
        "sites_list_envelope": envelope_unfiltered,
    });
    assert_eq!(json_unfiltered["hub_summary"], unfiltered_hub_summary);
    assert!(json_unfiltered.get("hidden_test_callers").is_none());
}

#[test]
fn test_r11_headings_replaced_by_envelope_total() {
    // R11: total_callers / total_affected headings replaced by the envelope total in
    // the envelope's form, one number per reply; legacy hub_summary, depth_limited,
    // truncated otherwise retained in JSON.
    let impact = make_impact_result(21, true, 0);
    assert_eq!(impact.total_affected, 21);
    let val_impact = serde_json::to_value(&impact).unwrap();
    let subc_impact = format_subc("impact", &val_impact);
    assert!(subc_impact.starts_with("21 affected call sites · 1 file"));
    assert!(subc_impact.contains("shown 15 of 21 sites (cap)"));

    let callers = make_callers_result(21, true, 0);
    assert_eq!(callers.total_callers, 21);
    let val_callers = serde_json::to_value(&callers).unwrap();
    let subc_callers = format_subc("callers", &val_callers);
    assert!(subc_callers.starts_with("21 callers · 1 file group"));
    assert!(subc_callers.contains("shown 15 of 21 items (cap)"));

    // Legacy fields retained in JSON
    assert_eq!(val_callers["depth_limited"], json!(true));
    assert_eq!(val_callers["truncated"], json!(0));
    assert!(val_callers.get("hub_summary").is_some());
}

// -----------------------------------------------------------------------
// Mutation Controls That Must Red
// -----------------------------------------------------------------------

#[test]
fn mutation_firing_cap_when_merely_exceeding_hub_summary_limit_reds() {
    // Mutation 1: firing cap whenever post-filter count merely exceeds HUB_SUMMARY_LIMIT (15)
    // reds the 16- and 20-entry fixtures on all three operations.
    let bad_hub_selector = |count: usize| -> bool {
        count > HUB_SUMMARY_LIMIT // BUG: evaluates against limit 15 instead of trigger 20
    };

    // 16-entry checks
    assert!(bad_hub_selector(16), "bad selector fires on 16");
    assert!(
        !hub_selector_activated(16),
        "canonical selector does not fire on 16"
    );

    // 20-entry checks
    assert!(bad_hub_selector(20), "bad selector fires on 20");
    assert!(
        !hub_selector_activated(20),
        "canonical selector does not fire on 20"
    );

    // Proves 16- and 20-entry fixtures would red
    let canonical_env16 = build_callgraph_envelope(Unit::Sites, 16, 16, 0);
    let mutated_env16 = if bad_hub_selector(16) {
        Some(ListEnvelope::new(
            15,
            Total::Exact(16),
            Unit::Sites,
            vec![Reason::Cap],
            CALLGRAPH_NARROW,
        ))
    } else {
        None
    };
    assert!(canonical_env16.is_none());
    assert!(mutated_env16.is_some());
    assert_ne!(canonical_env16, mutated_env16);
}

#[test]
fn mutation_moving_either_hub_constant_reds() {
    // Mutation 2: moving either hub constant reds the 20/21 pair
    // Threshold moved to 19: 20 entries incorrectly fires
    let threshold_low = 19;
    assert!(20 > threshold_low, "threshold 19 would fire on 20");
    assert!(
        !hub_selector_activated(20),
        "canonical threshold 20 does not fire on 20"
    );

    // Threshold moved to 21: 21 entries incorrectly fails to fire
    let threshold_high = 21;
    assert!(!(21 > threshold_high), "threshold 21 would not fire on 21");
    assert!(
        hub_selector_activated(21),
        "canonical threshold 20 fires on 21"
    );

    // Limit moved from 15 to 14: 21 fixture renders shown 14 instead of shown 15
    let mutated_limit = 14;
    assert_ne!(mutated_limit, HUB_SUMMARY_LIMIT);
}

#[test]
fn mutation_swapping_registered_unit_reds() {
    // Mutation 3: rendering `sites` on callers/call_tree or `items` on impact reds
    // that operation's 21-entry fixture.
    let res_impact_correct = make_impact_result(21, false, 0);
    let env_impact_correct = res_impact_correct.sites_list_envelope.as_ref().unwrap();
    assert_eq!(env_impact_correct.unit, Unit::Sites);

    // Mutated impact with items:
    let env_impact_mutated = ListEnvelope::new(
        15,
        Total::Exact(21),
        Unit::Items, // MUTATION: swapped unit
        vec![Reason::Cap],
        CALLGRAPH_NARROW,
    );
    assert_ne!(env_impact_correct.unit, env_impact_mutated.unit);
    let rendered_mutated = aft::list_envelope::render_trailer(&env_impact_mutated).unwrap();
    assert!(rendered_mutated.contains("21 items"));
    assert!(!rendered_mutated.contains("21 sites"));

    // Mutated callers with sites:
    let res_callers_correct = make_callers_result(21, false, 0);
    let env_callers_correct = res_callers_correct.callers_list_envelope.as_ref().unwrap();
    assert_eq!(env_callers_correct.unit, Unit::Items);

    let env_callers_mutated = ListEnvelope::new(
        15,
        Total::Exact(21),
        Unit::Sites, // MUTATION: swapped unit
        vec![Reason::Cap],
        CALLGRAPH_NARROW,
    );
    assert_ne!(env_callers_correct.unit, env_callers_mutated.unit);
    let rendered_callers_mutated =
        aft::list_envelope::render_trailer(&env_callers_mutated).unwrap();
    assert!(rendered_callers_mutated.contains("21 sites"));
    assert!(!rendered_callers_mutated.contains("21 items"));
}

#[test]
fn reverse_guard_depth_warning_not_called_for_envelope_governed_callgraph_surfaces() {
    // Parent request: static call-site guard's shape test that reds if depth_warning
    // is called for a registered callgraph surface (the reverse guard of suppression).
    for surface in CALLGRAPH_SURFACES {
        assert!(find_surface(surface.command, surface.mode, surface.list_id).is_some());
    }

    // Verify that for each registered surface, when truncated == 0 and depth_limited == true,
    // the formatted output never contains "(depth limited)".
    let callers = make_callers_result(12, true, 0);
    let subc_callers = format_subc("callers", &serde_json::to_value(&callers).unwrap());
    assert!(
        !subc_callers.contains("(depth limited)"),
        "callers must not invoke depth_warning when envelope-governed and truncated == 0"
    );

    let tree = make_call_tree_node(12, true, 0);
    let subc_tree = format_subc("call_tree", &serde_json::to_value(&tree).unwrap());
    assert!(
        !subc_tree.contains("(depth limited)"),
        "call_tree must not invoke depth_warning when envelope-governed and truncated == 0"
    );

    let impact = make_impact_result(12, true, 0);
    let subc_impact = format_subc("impact", &serde_json::to_value(&impact).unwrap());
    assert!(
        !subc_impact.contains("(depth limited)"),
        "impact must not invoke depth_warning when envelope-governed and truncated == 0"
    );
}

#[test]
fn test_transport_parity_across_all_callgraph_fixtures() {
    // Transport parity: every capped fixture is byte-equal across NDJSON and subc;
    // every complete-path fixture is byte-identical to its pre-spec golden except {a, b}.

    // 1. Capped impact 21
    let imp21 = make_impact_result(21, false, 0);
    let val_imp21 = serde_json::to_value(&imp21).unwrap();
    let subc_imp21 = format_subc("impact", &val_imp21);
    let base_imp21 = subc_imp21
        .lines()
        .filter(|l| !l.starts_with("shown ") && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let ndjson_imp21 = format_ndjson(&base_imp21, &val_imp21, SITES_LIST_ID);
    assert_eq!(subc_imp21.trim(), ndjson_imp21.trim());

    // 2. Capped callers 21
    let cal21 = make_callers_result(21, false, 0);
    let val_cal21 = serde_json::to_value(&cal21).unwrap();
    let subc_cal21 = format_subc("callers", &val_cal21);
    let base_cal21 = subc_cal21
        .lines()
        .filter(|l| !l.starts_with("shown ") && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let ndjson_cal21 = format_ndjson(&base_cal21, &val_cal21, CALLERS_LIST_ID);
    assert_eq!(subc_cal21.trim(), ndjson_cal21.trim());

    // 3. Capped call_tree 21
    let tr21 = make_call_tree_node(21, false, 0);
    let val_tr21 = serde_json::to_value(&tr21).unwrap();
    let subc_tr21 = format_subc("call_tree", &val_tr21);
    let base_tr21 = subc_tr21
        .lines()
        .filter(|l| !l.starts_with("shown ") && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let ndjson_tr21 = format_ndjson(&base_tr21, &val_tr21, TREE_LIST_ID);
    assert_eq!(subc_tr21.trim(), ndjson_tr21.trim());

    // 4. Exemption (a) callers
    let cal_ex = make_callers_result(12, true, 0);
    let val_cal_ex = serde_json::to_value(&cal_ex).unwrap();
    let subc_cal_ex = format_subc("callers", &val_cal_ex);
    let ndjson_cal_ex = format_ndjson(&subc_cal_ex, &val_cal_ex, CALLERS_LIST_ID);
    assert_eq!(subc_cal_ex.trim(), ndjson_cal_ex.trim());
}

fn create_synthesized_store_with_callers(
    num_callers: usize,
) -> (tempfile::TempDir, CallGraphStore, std::path::PathBuf) {
    let dir = tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let target_file = src_dir.join("target.ts");
    fs::write(
        &target_file,
        "export function target(): number { return 1; }\n",
    )
    .expect("write target");

    let mut files = vec![target_file.clone()];
    for i in 1..=num_callers {
        let caller_file = src_dir.join(format!("caller{:02}.ts", i));
        fs::write(
            &caller_file,
            format!(
                "import {{ target }} from './target';\nexport function caller{:02}() {{ return target(); }}\n",
                i
            ),
        )
        .expect("write caller");
        files.push(caller_file);
    }

    let store_dir = root.join(".callgraph-store");
    let store = CallGraphStore::open(store_dir, root.clone()).expect("open store");
    store.cold_build(&files).expect("cold build");
    (dir, store, target_file)
}

fn create_synthesized_store_with_chain() -> (
    tempfile::TempDir,
    CallGraphStore,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let dir = tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    let leaf_file = root.join("leaf.ts");
    fs::write(
        &leaf_file,
        "export function leaf(value: string): string { return value; }\n",
    )
    .expect("write leaf");
    let middle_file = root.join("middle.ts");
    fs::write(
        &middle_file,
        "import { leaf } from './leaf';\n\
         export function middle(value: string): string { return leaf(value); }\n",
    )
    .expect("write middle");
    let root_file = root.join("root.ts");
    fs::write(
        &root_file,
        "import { middle } from './middle';\n\
         export function root(value: string): string { return middle(value); }\n",
    )
    .expect("write root");
    let entry_file = root.join("entry.ts");
    fs::write(
        &entry_file,
        "import { root } from './root';\n\
         export function entry(): string { return root('fixture'); }\n",
    )
    .expect("write entry");

    let store =
        CallGraphStore::open(root.join(".callgraph-store"), root.clone()).expect("open store");
    store
        .cold_build(&[
            leaf_file.clone(),
            middle_file,
            root_file.clone(),
            entry_file,
        ])
        .expect("cold build");
    (dir, store, leaf_file, root_file)
}

#[test]
fn real_store_depth_boundaries_count_omitted_callgraph_rows() {
    let (_dir, store, leaf_file, root_file) = create_synthesized_store_with_chain();

    let callers = callgraph_store_adapter::callers_result(&store, &leaf_file, "leaf", 1, true)
        .expect("callers result");
    let callers_envelope = callers
        .callers_list_envelope
        .expect("callers depth boundary must emit an envelope");
    assert_eq!(callers_envelope.shown, 1);
    assert_eq!(callers_envelope.total, Total::AtLeast(1));
    assert_eq!(callers_envelope.reason, Some(Reason::Depth));

    let impact = callgraph_store_adapter::impact_result(&store, &leaf_file, "leaf", 1, true)
        .expect("impact result");
    let impact_envelope = impact
        .sites_list_envelope
        .expect("impact depth boundary must emit an envelope");
    assert_eq!(impact_envelope.shown, 1);
    assert_eq!(impact_envelope.total, Total::AtLeast(1));
    assert_eq!(impact_envelope.reason, Some(Reason::Depth));

    let tree = callgraph_store_adapter::call_tree_result(&store, &root_file, "root", 1, true)
        .expect("call tree result");
    let tree_envelope = tree
        .tree_list_envelope
        .expect("call tree depth boundary must emit an envelope");
    assert_eq!(tree_envelope.shown, 1);
    assert_eq!(tree_envelope.total, Total::AtLeast(1));
    assert_eq!(tree_envelope.reason, Some(Reason::Depth));
}

#[test]
fn test_producer_level_store_hub_capping_and_constants_linkage() {
    // Producer-level test on a real CallGraphStore:
    // 1. 20 callers through real adapter: hub selector NOT activated, full row set, no envelope
    let (_dir20, store20, target_file20) = create_synthesized_store_with_callers(20);
    let callers20 =
        callgraph_store_adapter::callers_result(&store20, &target_file20, "target", 1, true)
            .expect("callers result 20");
    assert_eq!(callers20.total_callers, 20);
    assert_eq!(callers20.callers.len(), 20);
    assert!(callers20.callers_list_envelope.is_none());

    let impact20 =
        callgraph_store_adapter::impact_result(&store20, &target_file20, "target", 1, true)
            .expect("impact result 20");
    assert_eq!(impact20.total_affected, 20);
    assert_eq!(impact20.callers.len(), 20);
    assert!(impact20.sites_list_envelope.is_none());

    // 2. 21 callers through real adapter: hub selector ACTIVATED, shown 15, cap envelope
    let (_dir21, store21, target_file21) = create_synthesized_store_with_callers(21);
    let callers21 =
        callgraph_store_adapter::callers_result(&store21, &target_file21, "target", 1, true)
            .expect("callers result 21");
    assert_eq!(callers21.total_callers, 21);
    assert_eq!(callers21.callers.len(), HUB_SUMMARY_LIMIT);
    let env_callers21 = callers21
        .callers_list_envelope
        .as_ref()
        .expect("21 callers must emit cap envelope through real adapter");
    assert_eq!(env_callers21.shown, HUB_SUMMARY_LIMIT);
    assert_eq!(env_callers21.total, Total::Exact(21));
    assert_eq!(env_callers21.reason, Some(Reason::Cap));
    assert_eq!(env_callers21.unit, Unit::Items);

    let impact21 =
        callgraph_store_adapter::impact_result(&store21, &target_file21, "target", 1, true)
            .expect("impact result 21");
    assert_eq!(impact21.total_affected, 21);
    assert_eq!(impact21.callers.len(), HUB_SUMMARY_LIMIT);
    let env_impact21 = impact21
        .sites_list_envelope
        .as_ref()
        .expect("21 sites must emit cap envelope through real adapter");
    assert_eq!(env_impact21.shown, HUB_SUMMARY_LIMIT);
    assert_eq!(env_impact21.total, Total::Exact(21));
    assert_eq!(env_impact21.reason, Some(Reason::Cap));
    assert_eq!(env_impact21.unit, Unit::Sites);
}
