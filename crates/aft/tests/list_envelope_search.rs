use aft::list_envelope::{derive_wire_key, render_trailer, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::search::{
    attach_projected_search_envelope, SEARCH_COMMAND, SEARCH_LIST_ID, SEARCH_WIRE_KEY,
};
use aft::list_surfaces::{find_surface, ReasonKind};
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde_json::{json, Value};

fn fixture_search_envelope(
    shown: usize,
    more_available: bool,
    engine_capped: bool,
) -> Option<ListEnvelope> {
    if !more_available && !engine_capped {
        return None;
    }
    let mut causes = Vec::new();
    if engine_capped {
        causes.push(Reason::Budget);
    }
    if more_available {
        causes.push(Reason::Cap);
    }
    Some(ListEnvelope::new(
        shown,
        Total::AtLeast(if more_available { shown + 1 } else { shown }),
        Unit::Results,
        causes,
        &["offset", "topK", "path", "includeTests"],
    ))
}

fn attach_fixture_search_envelope(
    map: &mut serde_json::Map<String, Value>,
    shown: usize,
    more_available: bool,
    engine_capped: bool,
) {
    if let Some(envelope) = fixture_search_envelope(shown, more_available, engine_capped) {
        attach_projected_search_envelope(map, &envelope);
    }
}

fn load_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/search/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture at {path}: {err}"));
    serde_json::from_str(&content)
        .unwrap_or_else(|err| panic!("failed to parse JSON from {path}: {err}"))
}

fn assert_no_bare_list_envelope(val: &Value) {
    match val {
        Value::Object(map) => {
            assert!(
                !map.contains_key("list_envelope"),
                "found forbidden bare key 'list_envelope'"
            );
            for v in map.values() {
                assert_no_bare_list_envelope(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                assert_no_bare_list_envelope(v);
            }
        }
        _ => {}
    }
}

#[test]
fn surface_registry_search_entry() {
    let surface = find_surface(SEARCH_COMMAND, "", SEARCH_LIST_ID)
        .expect("search surface must be registered");
    assert_eq!(surface.command, "search");
    assert_eq!(surface.list_id, "payload.results");
    assert_eq!(surface.unit, Unit::Results);
    assert_eq!(surface.narrow, &["offset", "topK", "path", "includeTests"]);

    // Engine projections emit Walk/Depth/Cap; legacy envelopes emit Budget/Cap.
    assert_eq!(surface.reasons.len(), 4);
    let walk_entry = surface
        .reasons
        .iter()
        .find(|r| r.reason == Reason::Walk)
        .expect("walk reason registered");
    assert_eq!(walk_entry.kind, ReasonKind::Bounding);
    assert!(walk_entry.predicate_name.contains("S2Exhausted"));

    let depth_entry = surface
        .reasons
        .iter()
        .find(|r| r.reason == Reason::Depth)
        .expect("depth reason registered");
    assert_eq!(depth_entry.kind, ReasonKind::Bounding);
    assert!(depth_entry.predicate_name.contains("S3DepthCap"));

    let budget_entry = surface
        .reasons
        .iter()
        .find(|r| r.reason == Reason::Budget)
        .expect("budget reason registered");
    assert_eq!(budget_entry.kind, ReasonKind::Bounding);
    assert!(budget_entry.predicate_name.contains("engine_capped"));

    let cap_entry = surface
        .reasons
        .iter()
        .find(|r| r.reason == Reason::Cap)
        .expect("cap reason registered");
    assert_eq!(cap_entry.kind, ReasonKind::Selecting);
    assert!(cap_entry.predicate_name.contains("more_available"));
}

#[test]
fn search_envelope_more_available_only_shown_10() {
    // Acceptance criteria:
    // `more_available` only, shown 10:
    // `shown 10 of ≥11 results (cap) · narrow: offset, topK, path, includeTests`,
    // `total = {"kind":"at_least","value":11}`, `causes: ["cap"]`.
    let envelope = fixture_search_envelope(10, true, false).expect("envelope produced");
    assert_eq!(envelope.shown, 10);
    assert_eq!(envelope.total, Total::AtLeast(11));
    assert_eq!(envelope.unit, Unit::Results);
    assert_eq!(envelope.reason, Some(Reason::Cap));
    assert_eq!(envelope.causes, vec![Reason::Cap]);
    assert_eq!(
        envelope.narrow,
        vec!["offset", "topK", "path", "includeTests"]
    );

    let rendered = render_trailer(&envelope).expect("trailer rendered");
    assert_eq!(
        rendered,
        "shown 10 of ≥11 results (cap) · narrow: offset, topK, path, includeTests"
    );

    let serialized = serde_json::to_value(&envelope).expect("serialize envelope");
    assert_eq!(
        serialized["total"],
        json!({ "kind": "at_least", "value": 11 })
    );
    assert_eq!(serialized["reason"], "cap");
    assert_eq!(serialized["causes"], json!(["cap"]));
    assert_eq!(serialized["unit"], "results");
    assert_eq!(
        serialized["narrow"],
        json!(["offset", "topK", "path", "includeTests"])
    );
}

#[test]
fn search_envelope_engine_capped_only_shown_10() {
    // Acceptance criteria:
    // `engine_capped` only, shown 10:
    // `shown 10 of ≥10 results (budget) · narrow: offset, topK, path, includeTests`,
    // `AtLeast(10)`, `causes: ["budget"]`.
    let envelope = fixture_search_envelope(10, false, true).expect("envelope produced");
    assert_eq!(envelope.shown, 10);
    assert_eq!(envelope.total, Total::AtLeast(10));
    assert_eq!(envelope.unit, Unit::Results);
    assert_eq!(envelope.reason, Some(Reason::Budget));
    assert_eq!(envelope.causes, vec![Reason::Budget]);
    assert_eq!(
        envelope.narrow,
        vec!["offset", "topK", "path", "includeTests"]
    );

    let rendered = render_trailer(&envelope).expect("trailer rendered");
    assert_eq!(
        rendered,
        "shown 10 of ≥10 results (budget) · narrow: offset, topK, path, includeTests"
    );

    let serialized = serde_json::to_value(&envelope).expect("serialize envelope");
    assert_eq!(
        serialized["total"],
        json!({ "kind": "at_least", "value": 10 })
    );
    assert_eq!(serialized["reason"], "budget");
    assert_eq!(serialized["causes"], json!(["budget"]));
    assert_eq!(serialized["unit"], "results");
}

#[test]
fn search_envelope_both_flags_shown_10_mixed_cause_schema_example() {
    // Acceptance criteria:
    // Both flags, shown 10:
    // `shown 10 of ≥11 results (budget) · narrow: offset, topK, path, includeTests`,
    // `AtLeast(11)`, `causes: ["budget","cap"]`;
    // this fixture is registered as the pinned mixed-cause wire example consumed by the envelope schema test.
    let envelope = fixture_search_envelope(10, true, true).expect("envelope produced");
    assert_eq!(envelope.shown, 10);
    assert_eq!(envelope.total, Total::AtLeast(11));
    assert_eq!(envelope.unit, Unit::Results);
    assert_eq!(envelope.reason, Some(Reason::Budget));
    assert_eq!(envelope.causes, vec![Reason::Budget, Reason::Cap]);
    assert_eq!(
        envelope.narrow,
        vec!["offset", "topK", "path", "includeTests"]
    );

    let rendered = render_trailer(&envelope).expect("trailer rendered");
    assert_eq!(
        rendered,
        "shown 10 of ≥11 results (budget) · narrow: offset, topK, path, includeTests"
    );

    // Validate wire schema per R15 pinned specification:
    // Exactly {"shown": <int>, "total": {"kind": "at_least", "value": <int>}, "unit": "results",
    // "reason": "budget", "causes": ["budget", "cap"], "narrow": ["offset", "topK", "path", "includeTests"]}
    let serialized = serde_json::to_value(&envelope).expect("serialize envelope");
    let obj = serialized.as_object().expect("envelope is an object");
    assert_eq!(obj.get("shown"), Some(&json!(10)));
    assert_eq!(
        obj.get("total"),
        Some(&json!({ "kind": "at_least", "value": 11 }))
    );
    assert_eq!(obj.get("unit"), Some(&json!("results")));
    assert_eq!(obj.get("reason"), Some(&json!("budget")));
    assert_eq!(obj.get("causes"), Some(&json!(["budget", "cap"])));
    assert_eq!(
        obj.get("narrow"),
        Some(&json!(["offset", "topK", "path", "includeTests"]))
    );

    // Causes must be ordered by precedence descending: Budget (rank 2) > Cap (rank 1)
    assert_eq!(envelope.causes[0], Reason::Budget);
    assert_eq!(envelope.causes[1], Reason::Cap);
    // reason == causes[0] always
    assert_eq!(envelope.reason, Some(envelope.causes[0]));
}

#[test]
fn search_complete_4_result_answer() {
    // Acceptance criteria:
    // A complete 4-result answer renders no trailer, serializes no envelope and stays byte-identical to its pre-spec golden.
    let envelope = fixture_search_envelope(4, false, false);
    assert!(
        envelope.is_none(),
        "complete answer must return no envelope"
    );

    let mut map = serde_json::Map::new();
    map.insert("query".into(), json!("foo"));
    attach_fixture_search_envelope(&mut map, 4, false, false);
    assert!(
        !map.contains_key("results_list_envelope"),
        "complete reply must not serialize results_list_envelope"
    );
}

#[test]
fn subc_format_search_more_available_only_suppresses_legacy_and_renders_trailer() {
    let mut data = json!({
        "text": "Found 10 result(s). More results available; raise topK to see more.",
        "results": (0..10).map(|i| json!({ "file": format!("src/{i}.rs") })).collect::<Vec<_>>(),
        "more_available": true,
        "engine_capped": false,
    });
    attach_fixture_search_envelope(data.as_object_mut().unwrap(), 10, true, false);

    let resp = Response {
        id: "1".into(),
        success: true,
        data,
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context(SEARCH_COMMAND, &resp, &ctx);

    assert!(
        formatted
            .contains("shown 10 of ≥11 results (cap) · narrow: offset, topK, path, includeTests"),
        "expected trailer in formatted output: {formatted}"
    );
    // Legacy honesty notes must be suppressed
    assert!(
        !formatted.contains("more results available"),
        "legacy 'more results available' should be suppressed: {formatted}"
    );
    assert!(
        !formatted.contains("enumeration capped"),
        "legacy 'enumeration capped' should be suppressed: {formatted}"
    );
}

#[test]
fn subc_format_search_engine_capped_only_suppresses_legacy_and_renders_trailer() {
    let mut data = json!({
        "text": "Found 10 result(s).",
        "results": (0..10).map(|i| json!({ "file": format!("src/{i}.rs") })).collect::<Vec<_>>(),
        "more_available": false,
        "engine_capped": true,
    });
    attach_fixture_search_envelope(data.as_object_mut().unwrap(), 10, false, true);

    let resp = Response {
        id: "2".into(),
        success: true,
        data,
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context(SEARCH_COMMAND, &resp, &ctx);

    assert!(
        formatted.contains(
            "shown 10 of ≥10 results (budget) · narrow: offset, topK, path, includeTests"
        ),
        "expected trailer in formatted output: {formatted}"
    );
    assert!(!formatted.contains("more results available"));
    assert!(!formatted.contains("enumeration capped"));
}

#[test]
fn subc_format_search_both_flags_preserves_status_text_and_json_markers() {
    // Acceptance criteria:
    // `fully_degraded` and `complete:false` asserted present as unchanged status text;
    // `more_available` and `engine_capped` retained in JSON.
    let mut data = json!({
        "text": "Found 10 result(s). More results available; raise topK to see more.",
        "results": (0..10).map(|i| json!({ "file": format!("src/{i}.rs") })).collect::<Vec<_>>(),
        "more_available": true,
        "engine_capped": true,
        "fully_degraded": true,
        "complete": false,
    });
    attach_fixture_search_envelope(data.as_object_mut().unwrap(), 10, true, true);

    // JSON flags must be retained
    assert_eq!(data["more_available"], true);
    assert_eq!(data["engine_capped"], true);
    assert_eq!(data["fully_degraded"], true);
    assert_eq!(data["complete"], false);

    let resp = Response {
        id: "3".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context(SEARCH_COMMAND, &resp, &ctx);

    // Trailer must be rendered
    assert!(
        formatted.contains(
            "shown 10 of ≥11 results (budget) · narrow: offset, topK, path, includeTests"
        ),
        "expected trailer: {formatted}"
    );
    // Legacy flags suppressed from note
    assert!(!formatted.contains("more results available"));
    assert!(!formatted.contains("enumeration capped"));
    // Status text preserved as unchanged
    assert!(
        formatted.contains("Search status: fully degraded; partial/incomplete."),
        "status note must remain: {formatted}"
    );
}

#[test]
fn complete_4_result_answer_is_byte_identical_to_pre_spec_golden() {
    // Acceptance criteria:
    // A complete 4-result answer renders no trailer, serializes no envelope and stays byte-identical to its pre-spec golden.
    let golden_text = "Found 4 result(s).";
    let mut data = json!({
        "text": golden_text,
        "results": [
            { "file": "src/a.rs" },
            { "file": "src/b.rs" },
            { "file": "src/c.rs" },
            { "file": "src/d.rs" },
        ],
        "more_available": false,
        "engine_capped": false,
        "fully_degraded": false,
        "complete": true,
    });
    attach_fixture_search_envelope(data.as_object_mut().unwrap(), 4, false, false);

    // Envelope not present
    assert!(data.get("results_list_envelope").is_none());

    let resp = Response {
        id: "4".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();

    // subc format matches golden byte-for-byte
    let formatted_subc = format_response_with_context(SEARCH_COMMAND, &resp, &ctx);
    assert_eq!(formatted_subc, golden_text);

    // NDJSON text matches golden byte-for-byte
    let formatted_ndjson = build_ndjson_text(golden_text, &data, Some("payload.results"), false);
    assert_eq!(formatted_ndjson, golden_text);
}

#[test]
fn transport_parity_search_fixtures() {
    // Transport parity: every capped fixture is byte-equal across NDJSON and subc
    let fixtures = [
        ("more_available_only", 10, true, false, false, true),
        ("engine_capped_only", 10, false, true, false, true),
        ("both_flags_mixed", 10, true, true, true, false),
    ];

    for (name, _shown, _more_available, _engine_capped, fully_degraded, complete) in fixtures {
        let fixture_val = load_fixture(name);
        let data = fixture_val["data"].clone();

        let resp = Response {
            id: fixture_val["id"].as_str().unwrap().into(),
            success: true,
            data: data.clone(),
        };
        let ctx = FormatContext::default();
        let subc_text = format_response_with_context(SEARCH_COMMAND, &resp, &ctx);

        let base_text = data["text"].as_str().unwrap();
        // If status note is present, ndjson applies it to base text
        let base_with_status = if fully_degraded || !complete {
            let mut notes = Vec::new();
            if fully_degraded {
                notes.push("fully degraded");
            }
            if !complete {
                notes.push("partial/incomplete");
            }
            format!("{base_text}\nSearch status: {}.", notes.join("; "))
        } else {
            base_text.to_string()
        };

        let ndjson_text = build_ndjson_text(&base_with_status, &data, Some(SEARCH_LIST_ID), false);

        assert_eq!(
            subc_text, ndjson_text,
            "transport parity mismatch for fixture '{name}': subc=\n{subc_text}\nndjson=\n{ndjson_text}"
        );
    }
}

#[test]
fn envelope_schema_test_search_fixtures() {
    let fixture_names = [
        "more_available_only",
        "engine_capped_only",
        "both_flags_mixed",
    ];

    for name in fixture_names {
        let val = load_fixture(name);
        let data = &val["data"];
        let env_val = &data[SEARCH_WIRE_KEY];

        // 1. Must parse into ListEnvelope
        let env: ListEnvelope = serde_json::from_value(env_val.clone())
            .unwrap_or_else(|err| panic!("fixture '{name}' envelope invalid: {err}"));

        // 2. reason == causes[0]
        assert_eq!(
            env.reason,
            Some(env.causes[0]),
            "fixture '{name}': reason must equal causes[0]"
        );

        // 3. causes in precedence order
        for i in 1..env.causes.len() {
            assert!(
                env.causes[i - 1].precedence() >= env.causes[i].precedence(),
                "fixture '{name}': causes must be in descending precedence"
            );
        }

        // 4. narrow order matches registered order
        assert_eq!(
            env.narrow,
            vec!["offset", "topK", "path", "includeTests"],
            "fixture '{name}': narrow order mismatch"
        );

        // 5. Unit matches registered unit
        assert_eq!(
            env.unit,
            Unit::Results,
            "fixture '{name}': unit must be Results"
        );
        assert_eq!(env.unit.as_str(), "results");

        // 6. Trailer text unit word equals envelope.unit
        let trailer = render_trailer(&env).expect("trailer renders");
        assert!(
            trailer.contains(" results ("),
            "fixture '{name}': trailer must contain unit 'results': {trailer}"
        );

        // 7. No top-level or nested bare 'list_envelope' key
        assert_no_bare_list_envelope(&val);

        // 8. Wire key derivable from registered list id
        let derived_key = derive_wire_key(SEARCH_LIST_ID, false);
        assert_eq!(derived_key, SEARCH_WIRE_KEY);
    }
}

#[test]
fn complete_fixture_from_disk_stays_byte_identical() {
    let val = load_fixture("complete_4_result");
    let data = &val["data"];
    assert!(
        data.get("results_list_envelope").is_none(),
        "complete fixture has no envelope"
    );

    let resp = Response {
        id: val["id"].as_str().unwrap().into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext::default();
    let formatted = format_response_with_context(SEARCH_COMMAND, &resp, &ctx);
    assert_eq!(formatted, "Found 4 result(s).");
}

#[test]
fn live_handle_semantic_search_attaches_envelope_when_more_available() {
    let project = tempfile::tempdir().expect("create project tempdir");
    let project_root = project.path();

    // Create 6 files with the needle
    for i in 1..=6 {
        let file_path = project_root.join(format!("src/file_{i}.rs"));
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, format!("fn search_target_func_{i}() {{}}\n")).unwrap();
    }

    let ctx = aft::context::AppContext::new(
        Box::new(aft::parser::TreeSitterProvider::new()),
        aft::config::Config {
            project_root: Some(project_root.to_path_buf()),
            ..aft::config::Config::default()
        },
    );
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        aft::context::SemanticIndexStatus::Disabled;

    // Search with top_k = 3 (less than 6 matches) -> more_available = true
    let req: aft::protocol::RawRequest = serde_json::from_value(json!({
        "id": "live-search-1",
        "command": "semantic_search",
        "query": "search_target_func",
        "top_k": 3,
    }))
    .unwrap();

    let resp = aft::commands::semantic_search::handle_semantic_search(&req, &ctx);
    assert!(resp.success);
    assert_eq!(resp.data["more_available"], true);
    assert_eq!(resp.data["engine_capped"], true);
    assert_eq!(resp.data["result_count"], 3);

    // The degraded walk owns the envelope projection; telemetry flags remain separate.
    let env_val = resp
        .data
        .get("results_list_envelope")
        .expect("envelope must be attached");
    let env: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();
    assert_eq!(env.shown, 3);
    assert_eq!(env.total, Total::AtLeast(4));
    assert_eq!(env.reason, Some(Reason::Walk));
    assert_eq!(env.causes, vec![Reason::Walk, Reason::Budget, Reason::Cap]);
    assert_eq!(env.unit, Unit::Results);

    // Formatted subc response renders trailer
    let formatted = format_response_with_context(SEARCH_COMMAND, &resp, &FormatContext::default());
    assert!(formatted
        .contains("shown 3 of ≥4 results (walk) · narrow: offset, topK, path, includeTests"));

    // Now search with top_k = 10 (greater than 6 matches) -> more_available = false, complete
    let req_complete: aft::protocol::RawRequest = serde_json::from_value(json!({
        "id": "live-search-2",
        "command": "semantic_search",
        "query": "search_target_func",
        "top_k": 10,
    }))
    .unwrap();

    let resp_complete = aft::commands::semantic_search::handle_semantic_search(&req_complete, &ctx);
    assert!(resp_complete.success);
    assert_eq!(resp_complete.data["more_available"], false);
    assert_eq!(resp_complete.data["engine_capped"], false);
    let complete_envelope: ListEnvelope =
        serde_json::from_value(resp_complete.data["results_list_envelope"].clone())
            .expect("degraded walk carries its shared envelope");
    assert_eq!(complete_envelope.reason, Some(Reason::Walk));
    assert_eq!(complete_envelope.total, Total::AtLeast(6));
    let formatted_complete =
        format_response_with_context(SEARCH_COMMAND, &resp_complete, &FormatContext::default());
    assert_eq!(
        formatted_complete
            .lines()
            .filter(|line| line.starts_with("shown "))
            .collect::<Vec<_>>(),
        ["shown 6 of ≥6 results (walk) · narrow: offset, topK, path, includeTests"]
    );
}
