use super::*;
use serde_json::json;

const CATALOG_JSON: &str = include_str!("../../../spec/feature-config/catalog.json");
const HARNESSES_JSON: &str = include_str!("../../../spec/feature-config/harnesses.json");

fn user(text: &str) -> ConfigInputs {
    ConfigInputs {
        user: Some(ConfigFile {
            path: PathBuf::from("/home/u/.config/cortexkit/aft.jsonc"),
            text: text.to_string(),
        }),
        project: None,
    }
}

fn with_project(mut inputs: ConfigInputs, text: &str) -> ConfigInputs {
    inputs.project = Some(ConfigFile {
        path: PathBuf::from("/repo/.cortexkit/aft.jsonc"),
        text: text.to_string(),
    });
    inputs
}

fn plan(inputs: &ConfigInputs, harness: Option<SetupHarness>) -> SetupPlan {
    derive_plan(inputs, harness, &NoRuntimeObservation)
        .expect("plan derives")
        .plan
}

fn plan_with(inputs: &ConfigInputs, observer: &dyn FeatureObserver) -> SetupPlan {
    derive_plan(inputs, None, observer)
        .expect("plan derives")
        .plan
}

/// Observer standing in for a running engine.
struct Observed {
    index: Effective,
    semantic_unsupported: Option<&'static str>,
    capability_blocked: Option<&'static str>,
}

impl FeatureObserver for Observed {
    fn index(&self, _plane: IndexPlane) -> IndexObservation {
        IndexObservation {
            effective: self.index,
            unavailable_reason: (self.index == Effective::Unavailable)
                .then(|| "backend_missing".to_string()),
        }
    }
    fn semantic_unsupported(&self) -> Option<String> {
        self.semantic_unsupported.map(str::to_string)
    }
    fn capability_blocked(&self, _id: &str) -> Option<String> {
        self.capability_blocked.map(str::to_string)
    }
}

fn observed(index: Effective) -> Observed {
    Observed {
        index,
        semantic_unsupported: None,
        capability_blocked: None,
    }
}

/// The row as a JSON object, so assertions read like the spec's matrix.
fn row_json(plan: &SetupPlan, id: &str) -> Value {
    serde_json::to_value(plan.feature(id).expect("catalog row")).unwrap()
}

fn matrix(plan: &SetupPlan, id: &str) -> Value {
    let row = row_json(plan, id);
    json!({
        "configured": row["configured"],
        "source": row["source"],
        "effective": row["effective"],
        "available": row["available"],
        "reason": row["reason"],
        "unavailable_reason": row["unavailable_reason"],
    })
}

#[test]
fn plan_tuples_equal_the_committed_catalog() {
    let catalog: Value = serde_json::from_str(CATALOG_JSON).unwrap();
    let expected = catalog["ordered_tuples"].as_array().unwrap();
    let emitted = serde_json::to_value(plan(&ConfigInputs::default(), None)).unwrap();
    assert_eq!(emitted["plan_version"], json!(1));
    let rows = emitted["features"].as_array().unwrap();
    let tuples: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row["id"],
                "kind": row["kind"],
                "group": row["group"],
                "order": row["order"],
                "binding": row["binding"],
                "prerequisites": row["prerequisites"],
            })
        })
        .collect();
    assert_eq!(&tuples, expected);

    let exclusions = catalog["exclusions"].as_array().unwrap();
    for excluded in exclusions.iter().filter_map(Value::as_str) {
        assert!(
            catalog_entry(excluded).is_none(),
            "{excluded} must not be a row"
        );
    }
}

#[test]
fn plan_rows_carry_exactly_the_v1_fields() {
    let emitted = serde_json::to_value(plan(&ConfigInputs::default(), None)).unwrap();
    let top: BTreeSet<&str> = emitted
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(top, BTreeSet::from(["plan_version", "features"]));
    let expected: BTreeSet<&str> = BTreeSet::from([
        "id",
        "kind",
        "group",
        "order",
        "label",
        "description",
        "binding",
        "default",
        "configured",
        "source",
        "proposed",
        "effective",
        "reason",
        "available",
        "unavailable_reason",
        "cost_note",
        "prerequisites",
    ]);
    for row in emitted["features"].as_array().unwrap() {
        let keys: BTreeSet<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, expected);
    }
    // Semantic indexing explains its download and CPU cost.
    let semantic = row_json(&plan(&ConfigInputs::default(), None), "indexes.semantic");
    let cost = semantic["cost_note"].as_str().unwrap();
    assert!(cost.contains("download") && cost.contains("CPU"), "{cost}");
}

#[test]
fn harness_selectors_equal_the_committed_list() {
    let harnesses: Value = serde_json::from_str(HARNESSES_JSON).unwrap();
    let accepted: Vec<&str> = harnesses["accepted_setup_selectors_today"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(accepted, SETUP_HARNESS_SELECTORS);
    for selector in SETUP_HARNESS_SELECTORS {
        assert_eq!(
            SetupHarness::from_selector(selector).unwrap().selector(),
            selector
        );
    }
    for rejected in [
        "opencode-v1",
        "opencode-v2",
        "runner",
        "mcp:x",
        "",
        "OpenCode",
    ] {
        assert!(SetupHarness::from_selector(rejected).is_err(), "{rejected}");
    }
    assert_eq!(SetupHarness::Omp.runtime_harness(), Harness::Pi);
}

#[test]
fn defaults_without_config_follow_the_matrix() {
    let plan = plan(&ConfigInputs::default(), None);
    assert_eq!(
        matrix(&plan, "aft_move"),
        json!({"configured": false, "source": "default", "effective": "off", "available": true, "reason": "default", "unavailable_reason": null})
    );
    assert_eq!(
        matrix(&plan, "aft_search"),
        json!({"configured": true, "source": "default", "effective": "ready", "available": true, "reason": "default", "unavailable_reason": null})
    );
    // Default-on unobserved index.
    assert_eq!(
        matrix(&plan, "indexes.trigram"),
        json!({"configured": true, "source": "default", "effective": "unavailable", "available": false, "reason": "default", "unavailable_reason": "runtime_not_observed"})
    );
    assert_eq!(
        matrix(&plan, "github.read"),
        json!({"configured": false, "source": "default", "effective": "off", "available": true, "reason": "default", "unavailable_reason": null})
    );
}

#[test]
fn a_present_base_list_is_a_configured_choice_for_every_tool() {
    let plan = plan(&user(r#"{"disabled_tools": []}"#), None);
    for feature in plan.features.iter().filter(|f| f.kind == FeatureKind::Tool) {
        assert_eq!(feature.source, "config", "{}", feature.id);
        assert_eq!(feature.reason, Some(REASON_CONFIGURED), "{}", feature.id);
        assert!(feature.configured, "{}", feature.id);
        assert_eq!(feature.effective, Effective::Ready, "{}", feature.id);
    }
}

#[test]
fn explicitly_enabled_indexes_report_configured_and_observed_states_keep_it() {
    let inputs = user(r#"{"indexes": {"trigram": true}}"#);
    assert_eq!(
        matrix(&plan(&inputs, None), "indexes.trigram"),
        json!({"configured": true, "source": "config", "effective": "unavailable", "available": false, "reason": "configured", "unavailable_reason": "runtime_not_observed"})
    );
    for state in [Effective::Ready, Effective::Building] {
        let plan = plan_with(&inputs, &observed(state));
        let row = matrix(&plan, "indexes.trigram");
        assert_eq!(row["effective"], json!(state));
        assert_eq!(row["available"], json!(true));
        assert_eq!(row["reason"], json!("configured"));
        assert_eq!(row["unavailable_reason"], Value::Null);
        assert_eq!(
            matrix(&plan, "indexes.callgraph")["reason"],
            json!("default")
        );
    }
}

#[test]
fn configured_off_unsupported_semantic_is_off_and_available() {
    let unsupported = Observed {
        index: Effective::Ready,
        semantic_unsupported: Some("semantic_backend_unsupported_platform"),
        capability_blocked: None,
    };
    let plan = plan_with(&user(r#"{"indexes": {"semantic": false}}"#), &unsupported);
    assert_eq!(
        matrix(&plan, "indexes.semantic"),
        json!({"configured": false, "source": "config", "effective": "off", "available": true, "reason": "configured", "unavailable_reason": null})
    );
    assert!(!plan.feature("indexes.semantic").unwrap().proposed);
}

#[test]
fn default_semantic_on_an_unsupported_platform_proposes_false_without_redefining_the_default() {
    let unsupported = Observed {
        index: Effective::Ready,
        semantic_unsupported: Some("semantic_backend_unsupported_platform"),
        capability_blocked: None,
    };
    let plan = plan_with(&ConfigInputs::default(), &unsupported);
    let semantic = plan.feature("indexes.semantic").unwrap();
    assert!(semantic.default && semantic.configured && !semantic.proposed);
    assert_eq!(semantic.source, "default");
    assert_eq!(semantic.effective, Effective::Unavailable);
    assert!(!semantic.available);
    assert_eq!(semantic.reason, Some(REASON_DEFAULT));
    assert_eq!(
        semantic.unavailable_reason.as_deref(),
        Some("semantic_backend_unsupported_platform")
    );
    // An explicit saved true keeps its proposed value.
    let explicit = plan_with(&user(r#"{"indexes": {"semantic": true}}"#), &unsupported);
    assert!(explicit.feature("indexes.semantic").unwrap().proposed);
}

#[test]
fn write_implied_read_row_is_exactly_the_literal_matrix_row() {
    let inputs = user(r#"{"github": {"write": true, "read": false}}"#);
    let plan = plan(&inputs, None);
    assert_eq!(
        matrix(&plan, "github.read"),
        json!({"configured": false, "source": "config", "effective": "ready", "available": true, "reason": "implied by github.write", "unavailable_reason": null})
    );
    assert_eq!(matrix(&plan, "github.write")["effective"], json!("ready"));

    // Read absent as well: same derivation, now default-sourced.
    let absent = plan_with(
        &user(r#"{"github": {"write": true}}"#),
        &NoRuntimeObservation,
    );
    let row = matrix(&absent, "github.read");
    assert_eq!(row["configured"], json!(false));
    assert_eq!(row["effective"], json!("ready"));
    assert_eq!(row["reason"], json!("implied by github.write"));

    // A missing runtime prerequisite keeps the derivation and names the cause.
    let blocked = Observed {
        index: Effective::Ready,
        semantic_unsupported: None,
        capability_blocked: Some("github_credentials_missing"),
    };
    let plan = plan_with(&inputs, &blocked);
    assert_eq!(
        matrix(&plan, "github.read"),
        json!({"configured": false, "source": "config", "effective": "unavailable", "available": false, "reason": "implied by github.write", "unavailable_reason": "github_credentials_missing"})
    );

    // Independently enabled read keeps its own derivation.
    let both = plan_with(
        &user(r#"{"github": {"write": true, "read": true}}"#),
        &NoRuntimeObservation,
    );
    assert_eq!(matrix(&both, "github.read")["reason"], json!("configured"));
}

#[test]
fn registered_runtime_blocked_tools_stay_ready_with_a_separate_cause() {
    let plan = plan(
        &user(
            r#"{"disabled_tools": [], "backup": {"enabled": false}, "inspect": {"enabled": false}, "bash": {"enabled": false}}"#,
        ),
        None,
    );
    for (id, cause) in [
        ("aft_safety", "backup_disabled"),
        ("aft_inspect", "inspect_disabled"),
        ("bash", "bash_disabled"),
    ] {
        assert_eq!(
            matrix(&plan, id),
            json!({"configured": true, "source": "config", "effective": "ready", "available": false, "reason": "configured", "unavailable_reason": cause}),
            "{id}"
        );
    }
    // The bash settings name the same runtime cause as the tool they configure.
    assert_eq!(
        matrix(&plan, "bash.compress"),
        json!({"configured": true, "source": "default", "effective": "ready", "available": false, "reason": "default", "unavailable_reason": "bash_disabled"})
    );
}

/// Same user base under two harness ids and inside/outside a project: the
/// effective state differs, base configured/source never does.
#[test]
fn harness_and_project_context_change_effective_state_but_not_base_choices() {
    let base = r#"{
        "disabled_tools": ["aft_move"],
        "harnesses": {
            "opencode": {"disabled_tools": ["aft_zoom"]},
            "pi": {"indexes": {"callgraph": false}}
        }
    }"#;
    let outside = user(base);
    let inside = with_project(
        user(base),
        r#"{"disabled_tools": ["aft_outline", "read"], "indexes": {"trigram": false, "semantic": true}}"#,
    );

    let opencode = plan(&outside, Some(SetupHarness::Opencode));
    let pi = plan(&outside, Some(SetupHarness::Pi));
    let omp = plan(&outside, Some(SetupHarness::Omp));
    let none = plan(&outside, None);
    let project = plan(&inside, Some(SetupHarness::Opencode));

    assert_eq!(
        opencode.feature("aft_zoom").unwrap().effective,
        Effective::Off
    );
    assert_eq!(pi.feature("aft_zoom").unwrap().effective, Effective::Ready);
    assert_eq!(
        none.feature("aft_zoom").unwrap().effective,
        Effective::Ready
    );
    assert_eq!(
        pi.feature("indexes.callgraph").unwrap().effective,
        Effective::Off
    );
    assert_eq!(
        pi.feature("indexes.callgraph").unwrap().reason,
        Some(REASON_CONFIGURED)
    );
    assert_eq!(
        opencode.feature("indexes.callgraph").unwrap().effective,
        Effective::Unavailable
    );
    assert_eq!(omp, pi, "omp resolves the pi harness block");

    // Inside the project: project disables and index offs apply; protected
    // slots and project re-enables are ignored.
    assert_eq!(
        project.feature("aft_outline").unwrap().effective,
        Effective::Off
    );
    assert_eq!(
        none.feature("aft_outline").unwrap().effective,
        Effective::Ready
    );
    assert_eq!(
        matrix(&project, "read"),
        json!({"configured": true, "source": "config", "effective": "ready", "available": true, "reason": "configured", "unavailable_reason": null})
    );
    assert_eq!(
        project.feature("indexes.trigram").unwrap().effective,
        Effective::Off
    );
    assert_eq!(
        project.feature("indexes.trigram").unwrap().reason,
        Some(REASON_CONFIGURED)
    );

    for (a, b) in [(&opencode, &pi), (&opencode, &project), (&pi, &none)] {
        for (left, right) in a.features.iter().zip(&b.features) {
            assert_eq!(
                (left.id, left.configured, left.source, left.default),
                (right.id, right.configured, right.source, right.default)
            );
        }
    }
}

#[test]
fn default_membership_disabled_by_a_later_tier_becomes_configured() {
    let inputs = with_project(
        ConfigInputs::default(),
        r#"{"disabled_tools": ["aft_zoom"]}"#,
    );
    let plan = plan(&inputs, None);
    assert_eq!(matrix(&plan, "aft_zoom")["reason"], json!("configured"));
    assert_eq!(matrix(&plan, "aft_zoom")["source"], json!("default"));
    assert_eq!(matrix(&plan, "aft_outline")["reason"], json!("default"));
}

#[test]
fn retired_keys_produce_a_plan_and_unparsable_files_do_not() {
    for doc in [
        r#"{"gh_read": {"enabled": true}}"#,
        r#"{"search_index": false}"#,
        r#"{"idle": {"lsp_ttl_minutes": 10}}"#,
    ] {
        assert!(
            derive_plan(&user(doc), None, &NoRuntimeObservation).is_ok(),
            "{doc}"
        );
    }
    assert!(derive_plan(&user("{ nope"), None, &NoRuntimeObservation).is_err());
}

#[test]
fn retired_inputs_count_as_configured_choices() {
    let plan = plan(
        &user(r#"{"tool_surface": "all", "semantic_search": false}"#),
        None,
    );
    assert_eq!(matrix(&plan, "aft_move")["source"], json!("config"));
    assert_eq!(matrix(&plan, "aft_move")["effective"], json!("ready"));
    assert_eq!(matrix(&plan, "indexes.semantic")["source"], json!("config"));
    assert_eq!(matrix(&plan, "indexes.semantic")["effective"], json!("off"));
}

#[test]
fn answers_are_validated_before_anything_is_written() {
    assert_eq!(
        parse_answers(r#"{"plan_version": 2, "selections": {}}"#).unwrap_err(),
        UNSUPPORTED_PLAN_VERSION
    );
    assert_eq!(
        parse_answers(r#"{"selections": {}}"#).unwrap_err(),
        UNSUPPORTED_PLAN_VERSION
    );
    assert_eq!(
        parse_answers(r#"{"plan_version": 1, "selections": {"aft_nope": true}}"#).unwrap_err(),
        "unknown_feature_id:aft_nope"
    );
    assert_eq!(
        parse_answers(r#"{"plan_version": 1, "selections": {"aft_move": "yes"}}"#).unwrap_err(),
        "invalid_selection_value:aft_move"
    );
    assert!(parse_answers(r#"{"plan_version": 1, "selections": ["aft_move"]}"#).is_err());
    assert_eq!(
        parse_answers(r#"{"plan_version": 1, "selections": {"aft_move": true}}"#).unwrap(),
        BTreeMap::from([("aft_move".to_string(), true)])
    );
}

fn written(existing: Option<&str>, selections: SetupSelections) -> (Value, SetupWrite) {
    written_with(existing, selections, &NoRuntimeObservation)
}

fn written_with(
    existing: Option<&str>,
    selections: SetupSelections,
    observer: &dyn FeatureObserver,
) -> (Value, SetupWrite) {
    let inputs = existing.map_or_else(ConfigInputs::default, user);
    let plan = plan_with(&inputs, observer);
    let write = render_setup(existing, &plan, &selections).unwrap();
    let value = serde_json::from_str(&strip_jsonc(&write.text)).unwrap();
    (value, write)
}

fn answers(pairs: &[(&str, bool)]) -> SetupSelections {
    SetupSelections::Answers(
        pairs
            .iter()
            .map(|(id, value)| ((*id).to_string(), *value))
            .collect(),
    )
}

#[test]
fn yes_on_a_missing_file_writes_the_proposed_defaults() {
    let (value, _) = written(None, SetupSelections::Yes);
    assert_eq!(
        value,
        json!({
            "disabled_tools": ["aft_delete", "aft_move"],
            "indexes": {"trigram": true, "semantic": true, "callgraph": true},
            "bash": {"compress": true, "rewrite": true, "background": true},
            "github": {"write": false, "read": false}
        })
    );
}

#[test]
fn yes_on_an_existing_file_keeps_explicit_choices_and_fills_the_rest() {
    let existing = r#"{
  // user comment
  "edit_mode": "hashline",
  "indexes": {"semantic": false},
  "github": {"read": true}
}"#;
    let (value, write) = written(Some(existing), SetupSelections::Yes);
    assert!(write.text.contains("// user comment"));
    assert_eq!(value["edit_mode"], json!("hashline"));
    assert_eq!(
        value["indexes"],
        json!({"semantic": false, "trigram": true, "callgraph": true})
    );
    assert_eq!(value["github"], json!({"read": true, "write": false}));
    assert_eq!(value["disabled_tools"], json!(["aft_delete", "aft_move"]));
    assert_eq!(
        value["bash"],
        json!({"compress": true, "rewrite": true, "background": true})
    );
}

/// Saving a write choice writes `github.read: true` next to it, so the file
/// shows both keys and the user can edit either. The resolver would turn read
/// on under write anyway; leaving the key absent hid that.
#[test]
fn write_choice_records_read_explicitly() {
    let (value, _) = written(Some(r#"{"github": {"write": true}}"#), SetupSelections::Yes);
    assert_eq!(value["github"], json!({"read": true, "write": true}));

    let (value, _) = written(None, answers(&[("github.write", true)]));
    assert_eq!(value["github"], json!({"read": true, "write": true}));

    let (value, _) = written(
        None,
        answers(&[("github.write", true), ("github.read", true)]),
    );
    assert_eq!(value["github"], json!({"read": true, "write": true}));

    // Turning read off turns write off with it.
    let (value, _) = written(
        Some(r#"{"github": {"read": true, "write": true}}"#),
        answers(&[("github.read", false)]),
    );
    assert_eq!(value["github"], json!({"read": false, "write": false}));

    // Both off are written too, not left out because they are the default.
    let (value, _) = written(
        None,
        answers(&[("github.write", false), ("github.read", false)]),
    );
    assert_eq!(value["github"], json!({"read": false, "write": false}));

    // A hand-written read false under write true is kept as written by yes;
    // the resolver still turns read on, which doctor reports.
    let (value, _) = written(
        Some(r#"{"github": {"read": false, "write": true}}"#),
        SetupSelections::Yes,
    );
    assert_eq!(value["github"], json!({"read": false, "write": true}));
    let resolved = plan(&user(&serde_json::to_string(&value).unwrap()), None);
    assert_eq!(
        resolved.feature("github.read").unwrap().effective,
        Effective::Ready
    );
}

#[test]
fn answers_that_turn_write_on_and_read_off_are_refused() {
    let error = parse_answers(
        r#"{"plan_version": 1, "selections": {"github.write": true, "github.read": false}}"#,
    )
    .unwrap_err();
    assert!(
        error.starts_with("conflicting_selections:github.write:github.read"),
        "{error}"
    );
    assert!(parse_answers(
        r#"{"plan_version": 1, "selections": {"github.write": false, "github.read": false}}"#,
    )
    .is_ok());
}

#[test]
fn partial_answers_preserve_omitted_base_fields() {
    let existing =
        r#"{"github": {"write": true}, "harnesses": {"pi": {"disabled_tools": ["aft_zoom"]}}}"#;
    let (value, _) = written(Some(existing), answers(&[("aft_move", true)]));
    assert_eq!(value["disabled_tools"], json!(["aft_delete"]));
    assert!(
        value.get("indexes").is_none(),
        "omitted indexes stay omitted"
    );
    assert_eq!(value["github"], json!({"write": true}));
    assert_eq!(
        value["harnesses"],
        json!({"pi": {"disabled_tools": ["aft_zoom"]}})
    );
}

#[test]
fn empty_desired_disables_write_a_literal_empty_list() {
    let (value, write) = written(None, answers(&[("aft_move", true), ("aft_delete", true)]));
    assert_eq!(value["disabled_tools"], json!([]));
    assert!(write.text.contains("\"disabled_tools\": []"));
    // `[]` and absence resolve differently: absence restores the default disables.
    let explicit = plan(&user(&write.text), None);
    assert_eq!(
        explicit.feature("aft_move").unwrap().effective,
        Effective::Ready
    );
    let absent = plan(&user("{}"), None);
    assert_eq!(
        absent.feature("aft_move").unwrap().effective,
        Effective::Off
    );
}

#[test]
fn unknown_disabled_entries_are_preserved_and_reported() {
    let existing = r#"{"disabled_tools": ["aft_move", "aft_future_tool", "typo_name"]}"#;
    for selections in [
        SetupSelections::Yes,
        answers(&[("aft_move", true)]),
        answers(&[("aft_outline", true), ("aft_search", true)]),
    ] {
        let (value, write) = written(Some(existing), selections);
        let list: Vec<&str> = value["disabled_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            list.contains(&"aft_future_tool") && list.contains(&"typo_name"),
            "{list:?}"
        );
        assert_eq!(
            write.unknown_disabled_tools,
            vec!["aft_future_tool", "typo_name"]
        );
    }
    let outcome = derive_plan(&user(existing), None, &NoRuntimeObservation).unwrap();
    assert_eq!(
        outcome.unknown_disabled_tools,
        vec!["aft_future_tool", "typo_name"]
    );
    let warning = unknown_disabled_warning(&outcome.unknown_disabled_tools).unwrap();
    assert!(
        warning.starts_with("unknown_disabled_tools:")
            && warning.ends_with("aft_future_tool, typo_name")
    );
}

#[test]
fn historical_aliases_are_written_canonically() {
    let (value, write) = written(
        Some(r#"{"disabled_tools": ["aft_glob", "aft_move"]}"#),
        SetupSelections::Yes,
    );
    assert_eq!(value["disabled_tools"], json!(["aft_move", "glob"]));
    assert!(write.unknown_disabled_tools.is_empty());
}

#[test]
fn unsupported_default_semantic_is_saved_false_only_when_the_save_covers_it() {
    let unsupported = Observed {
        index: Effective::Ready,
        semantic_unsupported: Some("semantic_backend_unsupported_platform"),
        capability_blocked: None,
    };
    let (value, _) = written_with(None, SetupSelections::Yes, &unsupported);
    assert_eq!(value["indexes"]["semantic"], json!(false));
    let (value, _) = written_with(Some("{}"), answers(&[("aft_move", true)]), &unsupported);
    assert!(value.get("indexes").is_none());
    let (value, _) = written_with(
        Some(r#"{"indexes": {"semantic": true}}"#),
        SetupSelections::Yes,
        &unsupported,
    );
    assert_eq!(value["indexes"]["semantic"], json!(true));
}

#[test]
fn setup_writes_only_the_base_and_keeps_comments_and_unrelated_keys() {
    let existing = "{\n  // why hashline\n  \"edit_mode\": \"hashline\",\n  /* block */\n  \"harnesses\": {\"opencode\": {\"indexes\": {\"semantic\": false}}},\n}\n";
    let (value, write) = written(Some(existing), answers(&[("indexes.semantic", true)]));
    assert!(write.text.contains("// why hashline") && write.text.contains("/* block */"));
    assert_eq!(value["indexes"], json!({"semantic": true}));
    assert_eq!(
        value["harnesses"],
        json!({"opencode": {"indexes": {"semantic": false}}})
    );
}

#[test]
fn bash_companions_are_not_rows_and_bash_settings_are() {
    for companion in ["bash_status", "bash_write", "bash_watch", "bash_kill"] {
        assert!(catalog_entry(companion).is_none(), "{companion}");
        assert_eq!(
            parse_answers(&format!(
                r#"{{"plan_version": 1, "selections": {{"{companion}": true}}}}"#
            ))
            .unwrap_err(),
            format!("unknown_feature_id:{companion}")
        );
    }
    let plan = plan(&ConfigInputs::default(), None);
    let shell: Vec<(&str, FeatureKind)> = plan
        .features
        .iter()
        .filter(|feature| feature.group == "Shell")
        .map(|feature| (feature.id, feature.kind))
        .collect();
    assert_eq!(
        shell,
        vec![
            ("bash", FeatureKind::Tool),
            ("bash.compress", FeatureKind::Setting),
            ("bash.rewrite", FeatureKind::Setting),
            ("bash.background", FeatureKind::Setting),
        ]
    );
    for id in ["bash.compress", "bash.rewrite", "bash.background"] {
        let row = row_json(&plan, id);
        assert_eq!(row["kind"], json!("setting"));
        assert_eq!(row["binding"], json!({"path": id, "tool_name": null}));
        assert_eq!(row["prerequisites"], json!(["bash"]));
        assert_eq!(row["default"], json!(true));
        assert_eq!(row["proposed"], json!(true));
        assert_eq!(
            matrix(&plan, id),
            json!({"configured": true, "source": "default", "effective": "ready", "available": true, "reason": "default", "unavailable_reason": null}),
            "{id}"
        );
    }
    // GitHub rows describe what each lets the agent do, and write needs read.
    let read = row_json(&plan, "github.read");
    let write = row_json(&plan, "github.write");
    assert_eq!(read["label"], json!("Read"));
    assert_eq!(write["label"], json!("Write"));
    assert!(read["description"].as_str().unwrap().contains("issue://N"));
    assert!(write["description"]
        .as_str()
        .unwrap()
        .contains("issue://N/comments/K"));
    assert_eq!(write["prerequisites"], json!(["github.read"]));
    assert_eq!(read["prerequisites"], json!([]));
}

#[test]
fn bash_settings_follow_every_config_shape() {
    let cases = [
        (
            r#"{"bash": {"compress": false}}"#,
            false,
            true,
            "configured",
        ),
        (r#"{"bash": false}"#, false, false, "configured"),
        (r#"{"bash": true}"#, true, true, "configured"),
        (
            r#"{"experimental": {"bash": {"rewrite": true}}}"#,
            false,
            true,
            "configured",
        ),
    ];
    for (text, compress, rewrite, reason) in cases {
        let plan = plan(&user(text), None);
        let row = plan.feature("bash.compress").unwrap();
        assert_eq!(row.configured, compress, "{text}");
        assert_eq!(row.source, "config", "{text}");
        assert_eq!(row.reason, Some(reason), "{text}");
        assert_eq!(
            row.effective,
            if compress {
                Effective::Ready
            } else {
                Effective::Off
            },
            "{text}"
        );
        assert_eq!(
            plan.feature("bash.rewrite").unwrap().configured,
            rewrite,
            "{text}"
        );
    }
    // A project that turns a setting off makes it configured without
    // changing the user's base choice.
    let project = plan(
        &with_project(ConfigInputs::default(), r#"{"bash": {"rewrite": false}}"#),
        None,
    );
    let rewrite = project.feature("bash.rewrite").unwrap();
    assert_eq!(
        (
            rewrite.configured,
            rewrite.source,
            rewrite.effective,
            rewrite.reason
        ),
        (true, "default", Effective::Off, Some(REASON_CONFIGURED))
    );
}

#[test]
fn bash_settings_are_off_while_the_bash_tool_is_disabled() {
    let plan = plan(&user(r#"{"disabled_tools": ["bash"]}"#), None);
    assert_eq!(plan.feature("bash").unwrap().effective, Effective::Off);
    for id in ["bash.compress", "bash.rewrite", "bash.background"] {
        assert_eq!(
            matrix(&plan, id),
            json!({"configured": true, "source": "default", "effective": "off", "available": true, "reason": "requires bash", "unavailable_reason": null}),
            "{id}"
        );
    }
}

#[test]
fn setup_writes_bash_settings_explicitly() {
    let (value, _) = written(
        None,
        answers(&[
            ("bash", true),
            ("bash.compress", true),
            ("bash.rewrite", false),
            ("bash.background", true),
        ]),
    );
    assert_eq!(
        value["bash"],
        json!({"compress": true, "rewrite": false, "background": true})
    );
    // Existing bash keys that setup does not manage are kept.
    let (value, _) = written(
        Some(r#"{"bash": {"linux_scope": true, "compress": false}}"#),
        answers(&[("bash.compress", true)]),
    );
    assert_eq!(
        value["bash"],
        json!({"linux_scope": true, "compress": true})
    );
    // Yes keeps an explicit choice and fills the rest.
    let (value, _) = written(
        Some(r#"{"bash": {"background": false}}"#),
        SetupSelections::Yes,
    );
    assert_eq!(
        value["bash"],
        json!({"background": false, "compress": true, "rewrite": true})
    );
}

/// A boolean `bash` is spelled out as the object it resolves to before one
/// setting changes, so the runtime gate and the other settings keep their
/// meaning.
#[test]
fn a_boolean_bash_is_expanded_before_a_setting_changes() {
    let (value, _) = written(
        Some(r#"{"disabled_tools": [], "bash": false}"#),
        answers(&[("bash.compress", true)]),
    );
    assert_eq!(
        value["bash"],
        json!({"enabled": false, "compress": true, "rewrite": false, "background": false})
    );
    let (value, _) = written(
        Some(r#"{"disabled_tools": [], "bash": true}"#),
        answers(&[("bash.rewrite", false)]),
    );
    assert_eq!(
        value["bash"],
        json!({"enabled": true, "compress": true, "rewrite": false, "background": true})
    );
    // Yes leaves a boolean alone: every setting is already an explicit choice.
    let (value, _) = written(
        Some(r#"{"disabled_tools": [], "bash": true}"#),
        SetupSelections::Yes,
    );
    assert_eq!(value["bash"], json!(true));
}

/// The companions follow `bash`: setup never adds one to `disabled_tools`,
/// and an existing entry stays as the user wrote it.
#[test]
fn setup_never_adds_bash_companions_and_keeps_existing_ones() {
    let companions = ["bash_kill", "bash_status", "bash_watch", "bash_write"];
    let no_companion = |value: &Value| {
        value["disabled_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .all(|name| !companions.contains(&name))
    };
    for selections in [
        SetupSelections::Yes,
        answers(&[("bash", false)]),
        answers(&[
            ("bash", false),
            ("bash.background", false),
            ("bash.compress", false),
            ("bash.rewrite", false),
        ]),
    ] {
        let (value, _) = written(None, selections.clone());
        assert!(no_companion(&value), "{value}");
        let (value, _) = written(Some(r#"{"disabled_tools": ["bash"]}"#), selections);
        assert!(no_companion(&value), "{value}");
    }
    let (value, _) = written(None, answers(&[("bash", false)]));
    assert_eq!(
        value["disabled_tools"],
        json!(["aft_delete", "aft_move", "bash"])
    );

    let existing = r#"{"disabled_tools": ["bash_status", "aft_move", "bash_kill", "typo_name"]}"#;
    for selections in [
        SetupSelections::Yes,
        answers(&[("bash", true)]),
        answers(&[("bash", false), ("aft_move", true)]),
    ] {
        let (value, write) = written(Some(existing), selections);
        let list: Vec<&str> = value["disabled_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            list.contains(&"bash_status") && list.contains(&"bash_kill"),
            "{list:?}"
        );
        assert!(!list.contains(&"bash_watch") && !list.contains(&"bash_write"));
        assert_eq!(write.unknown_disabled_tools, vec!["typo_name"]);
    }
}
