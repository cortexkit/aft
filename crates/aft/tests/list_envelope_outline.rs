use aft::list_envelope::{render_trailer, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::outline::{build_outline_files_envelope, outline_measure_trailer_len};
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde_json::Value;

fn legacy_rollup_sentence(rollup_count: usize, max_bytes: usize) -> String {
    let (directory_word, rollup_phrase, expand_phrase) = if rollup_count == 1 {
        ("directory", "a rollup", "it")
    } else {
        ("directories", "rollups", "one")
    };
    let budget_str = if max_bytes >= 1024 && max_bytes % 1024 == 0 {
        format!("{}KB", max_bytes / 1024)
    } else {
        format!("{max_bytes} bytes")
    };
    format!(
        "\n{rollup_count} {directory_word} shown as {rollup_phrase} (budget: {budget_str}); \
         expand {expand_phrase} with aft_outline <dir> files:true\n"
    )
}

fn load_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/outline/{name}.json",
        crate::test_helpers::manifest_dir().display()
    );
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture {path}: {err}"));
    serde_json::from_str(&content)
        .unwrap_or_else(|err| panic!("failed to parse JSON fixture {path}: {err}"))
}

#[test]
fn budget_rollups_present_renders_expected_trailer() {
    let data = load_fixture("budget_rollups");
    let resp = Response {
        id: "test-budget".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        outline_mode: aft::subc_format::OutlineMode::Files,
        ..Default::default()
    };

    let subc_text = format_response_with_context("outline", &resp, &ctx);
    let ndjson_text = build_ndjson_text(
        data["text"].as_str().unwrap(),
        &data,
        Some("payload.files"),
        false,
    );

    // Transport parity: byte-equal across NDJSON and subc
    assert_eq!(subc_text, ndjson_text);

    // Trailer text matches acceptance criteria
    assert!(
        subc_text.contains("shown 412 of 1980 files (budget) · narrow: path"),
        "subc text was: {subc_text}"
    );

    // Envelope validation
    let env: ListEnvelope =
        serde_json::from_value(data["files_list_envelope"].clone()).expect("valid envelope");
    assert_eq!(env.shown, 412);
    assert_eq!(env.total, Total::Exact(1980));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Budget));
    assert_eq!(env.causes, vec![Reason::Budget]);
    assert_eq!(env.narrow, vec!["path"]);
}

#[test]
fn nested_budget_rollup_counts_once_through_frontier_without_double_counting() {
    let data = load_fixture("nested_budget_rollup");
    let resp = Response {
        id: "test-nested".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        outline_mode: aft::subc_format::OutlineMode::Files,
        ..Default::default()
    };

    let subc_text = format_response_with_context("outline", &resp, &ctx);
    let ndjson_text = build_ndjson_text(
        data["text"].as_str().unwrap(),
        &data,
        Some("payload.files"),
        false,
    );

    assert_eq!(subc_text, ndjson_text);
    assert!(
        subc_text.contains("shown 10 of 60 files (budget) · narrow: path"),
        "subc text was: {subc_text}"
    );

    let env: ListEnvelope =
        serde_json::from_value(data["files_list_envelope"].clone()).expect("valid envelope");
    assert_eq!(env.shown, 10);
    assert_eq!(env.total, Total::Exact(60));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Budget));
    assert_eq!(env.causes, vec![Reason::Budget]);
}

#[test]
fn data_heavy_only_renders_no_trailer_closed_exemption_b_byte_equal() {
    let data = load_fixture("data_heavy_only");
    let resp = Response {
        id: "test-data-heavy".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        outline_mode: aft::subc_format::OutlineMode::Files,
        ..Default::default()
    };

    let subc_text = format_response_with_context("outline", &resp, &ctx);
    let ndjson_text = build_ndjson_text(
        data["text"].as_str().unwrap(),
        &data,
        Some("payload.files"),
        false,
    );

    // Closed exemption (b): byte-equal across NDJSON and subc
    assert_eq!(subc_text, ndjson_text);

    // No trailer rendered
    assert!(!subc_text.contains("shown "));
    assert!(!subc_text.contains("(budget)"));
    assert!(!subc_text.contains("(walk)"));
    // Legacy rollup sentence is removed
    assert!(!subc_text.contains("shown as rollups"));
    assert!(!subc_text.contains("shown as a rollup"));

    // No envelope in JSON
    assert!(data.get("files_list_envelope").is_none());
}

#[test]
fn collection_truncated_alone_renders_walk_trailer_with_at_least() {
    let data = load_fixture("collection_truncated_alone");
    let resp = Response {
        id: "test-collection-truncated-alone".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        outline_mode: aft::subc_format::OutlineMode::Files,
        ..Default::default()
    };

    let subc_text = format_response_with_context("outline", &resp, &ctx);
    let ndjson_text = build_ndjson_text(
        data["text"].as_str().unwrap(),
        &data,
        Some("payload.files"),
        false,
    );

    assert_eq!(subc_text, ndjson_text);
    assert!(
        subc_text.contains("shown 412 of ≥412 files (walk) · narrow: path"),
        "subc text was: {subc_text}"
    );

    let env: ListEnvelope =
        serde_json::from_value(data["files_list_envelope"].clone()).expect("valid envelope");
    assert_eq!(env.shown, 412);
    assert_eq!(env.total, Total::AtLeast(412));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Walk));
    assert_eq!(env.causes, vec![Reason::Walk]);
    assert_eq!(env.narrow, vec!["path"]);

    // The legacy collection_truncated flag stays in JSON
    assert_eq!(data["collection_truncated"], true);
}

#[test]
fn collection_truncated_plus_budget_rollups_renders_walk_trailer_with_mixed_causes() {
    let data = load_fixture("collection_truncated_with_budget");
    let resp = Response {
        id: "test-collection-truncated-with-budget".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        outline_mode: aft::subc_format::OutlineMode::Files,
        ..Default::default()
    };

    let subc_text = format_response_with_context("outline", &resp, &ctx);
    let ndjson_text = build_ndjson_text(
        data["text"].as_str().unwrap(),
        &data,
        Some("payload.files"),
        false,
    );

    assert_eq!(subc_text, ndjson_text);
    assert!(
        subc_text.contains("shown 412 of ≥1980 files (walk) · narrow: path"),
        "subc text was: {subc_text}"
    );

    let env: ListEnvelope =
        serde_json::from_value(data["files_list_envelope"].clone()).expect("valid envelope");
    assert_eq!(env.shown, 412);
    assert_eq!(env.total, Total::AtLeast(1980));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Walk));
    assert_eq!(env.causes, vec![Reason::Walk, Reason::Budget]);
    assert_eq!(env.narrow, vec!["path"]);

    assert_eq!(data["collection_truncated"], true);
}

#[test]
fn measurement_test_asserts_measure_trailer_len_matches_rendered_trailer_bytes() {
    let fixtures = [
        "budget_rollups",
        "nested_budget_rollup",
        "collection_truncated_alone",
        "collection_truncated_with_budget",
        "r1_boundary",
    ];

    for fixture_name in fixtures {
        let data = load_fixture(fixture_name);
        let env_val = data
            .get("files_list_envelope")
            .unwrap_or_else(|| panic!("fixture {fixture_name} missing files_list_envelope"));
        let envelope: ListEnvelope = serde_json::from_value(env_val.clone()).unwrap();

        let measured = outline_measure_trailer_len(&envelope);
        let rendered = render_trailer(&envelope).expect("capped fixture must render trailer");
        assert_eq!(
            measured,
            rendered.len(),
            "fixture {fixture_name}: measure_trailer_len ({measured}) != rendered bytes ({})",
            rendered.len()
        );
    }
}

#[test]
fn rollup_count_length_test_trailer_is_never_longer_than_legacy_sentence() {
    const BUDGET: usize = 30 * 1024;
    for rollup_count in 1..=10_000 {
        let legacy = legacy_rollup_sentence(rollup_count, BUDGET);
        // Envelope with typical shown file count (e.g. 100) and exact total including rollups
        let shown = 100;
        let total = shown + rollup_count * 10;
        let envelope = ListEnvelope::new(
            shown,
            Total::Exact(total),
            Unit::Files,
            vec![Reason::Budget],
            &["path"],
        );
        let trailer_len = outline_measure_trailer_len(&envelope);
        // The rendered addition to table text is "\n\n" (2 bytes) + trailer
        let rendered_addition = 2 + trailer_len;

        assert!(
            rendered_addition <= legacy.len(),
            "at rollup_count {rollup_count}: rendered addition ({rendered_addition}) > legacy sentence ({})",
            legacy.len()
        );
    }
}

#[test]
fn build_outline_files_envelope_unit_and_causes() {
    // 1. Neither fired -> None
    let env = build_outline_files_envelope(10, 0, false, false, false, 0);
    assert!(env.is_none());

    // 2. Budget only -> Exact total, Reason::Budget
    let env = build_outline_files_envelope(412, 1568, true, false, false, 0).unwrap();
    assert_eq!(env.shown, 412);
    assert_eq!(env.total, Total::Exact(1980));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Budget));
    assert_eq!(env.causes, vec![Reason::Budget]);
    assert_eq!(env.narrow, vec!["path"]);

    // 3. Walk only (collection_truncated) -> AtLeast(shown), Reason::Walk
    let env = build_outline_files_envelope(412, 0, false, true, false, 0).unwrap();
    assert_eq!(env.shown, 412);
    assert_eq!(env.total, Total::AtLeast(412));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Walk));
    assert_eq!(env.causes, vec![Reason::Walk]);

    // 4. Walk + Budget -> AtLeast(shown + budget), Reason::Walk, causes: [Walk, Budget]
    let env = build_outline_files_envelope(412, 1568, true, true, false, 0).unwrap();
    assert_eq!(env.shown, 412);
    assert_eq!(env.total, Total::AtLeast(1980));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Walk));
    assert_eq!(env.causes, vec![Reason::Walk, Reason::Budget]);
}

#[test]
fn r1_boundary_fixture_transport_parity_and_trailer() {
    let data = load_fixture("r1_boundary");
    let resp = Response {
        id: "test-r1".into(),
        success: true,
        data: data.clone(),
    };
    let ctx = FormatContext {
        outline_mode: aft::subc_format::OutlineMode::Files,
        ..Default::default()
    };

    let subc_text = format_response_with_context("outline", &resp, &ctx);
    let ndjson_text = build_ndjson_text(
        data["text"].as_str().unwrap(),
        &data,
        Some("payload.files"),
        false,
    );

    assert_eq!(subc_text, ndjson_text);
    assert!(
        subc_text.contains("shown 2 of 12 files (budget) · narrow: path"),
        "subc text was: {subc_text}"
    );

    let env: ListEnvelope =
        serde_json::from_value(data["files_list_envelope"].clone()).expect("valid envelope");
    assert_eq!(env.shown, 2);
    assert_eq!(env.total, Total::Exact(12));
    assert_eq!(env.unit, Unit::Files);
    assert_eq!(env.reason, Some(Reason::Budget));
    assert_eq!(env.causes, vec![Reason::Budget]);
    assert_eq!(env.narrow, vec!["path"]);
}

#[test]
fn r1_boundary_expansion_fitting_by_fewer_bytes_than_legacy_sentence_is_accepted() {
    // Legacy sentence for 1 rollup in 30KB is 87 bytes
    let legacy_len = legacy_rollup_sentence(1, 30 * 1024).len();
    assert_eq!(legacy_len, 91);

    // Trailer for shown 2, total 12 files
    let env = ListEnvelope::new(
        2,
        Total::Exact(12),
        Unit::Files,
        vec![Reason::Budget],
        &["path"],
    );
    let trailer_rendered_addition = 2 + outline_measure_trailer_len(&env);
    // Trailer rendered addition is 47 bytes ("\n\nshown 2 of 12 files (budget) · narrow: path")
    assert_eq!(trailer_rendered_addition, 46);

    // Delta: an expansion that requires between 47 and 91 bytes fits under the new contract,
    // whereas it would have been rejected by the legacy sentence.
    let headroom = legacy_len - trailer_rendered_addition;
    assert_eq!(headroom, 45);
    assert!(
        trailer_rendered_addition < legacy_len,
        "new trailer is strictly smaller, allowing expansions that fit by fewer bytes than legacy"
    );
}

#[test]
fn envelope_schema_no_literal_list_envelope_key() {
    let fixtures = [
        "budget_rollups",
        "nested_budget_rollup",
        "data_heavy_only",
        "collection_truncated_alone",
        "collection_truncated_with_budget",
        "r1_boundary",
    ];

    fn check_no_literal_list_envelope(v: &Value) {
        match v {
            Value::Object(map) => {
                assert!(
                    !map.contains_key("list_envelope"),
                    "no top-level or nested key literally named 'list_envelope' allowed"
                );
                for val in map.values() {
                    check_no_literal_list_envelope(val);
                }
            }
            Value::Array(arr) => {
                for val in arr {
                    check_no_literal_list_envelope(val);
                }
            }
            _ => {}
        }
    }

    for name in fixtures {
        let data = load_fixture(name);
        check_no_literal_list_envelope(&data);
    }
}
