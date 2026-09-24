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
    derive_plan(inputs, harness, &NoRuntimeObservation, PolicyPhase::Window)
        .expect("plan derives")
        .plan
}

fn plan_with(inputs: &ConfigInputs, observer: &dyn FeatureObserver) -> SetupPlan {
    derive_plan(inputs, None, observer, PolicyPhase::Window)
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
            unavailable_reason: (self.index == Effective::Unavailable).then(|| "backend_missing".to_string()),
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
        assert!(catalog_entry(excluded).is_none(), "{excluded} must not be a row");
    }
}

#[test]
fn plan_rows_carry_exactly_the_v1_fields() {
    let emitted = serde_json::to_value(plan(&ConfigInputs::default(), None)).unwrap();
    let top: BTreeSet<&str> = emitted.as_object().unwrap().keys().map(String::as_str).collect();
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
        let keys: BTreeSet<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
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
        assert_eq!(SetupHarness::from_selector(selector).unwrap().selector(), selector);
    }
    for rejected in ["opencode-v1", "opencode-v2", "runner", "mcp:x", "", "OpenCode"] {
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
        assert_eq!(matrix(&plan, "indexes.callgraph")["reason"], json!("default"));
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
    let absent = plan_with(&user(r#"{"github": {"write": true}}"#), &NoRuntimeObservation);
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
    let both = plan_with(&user(r#"{"github": {"write": true, "read": true}}"#), &NoRuntimeObservation);
    assert_eq!(matrix(&both, "github.read")["reason"], json!("configured"));
}

#[test]
fn registered_runtime_blocked_tools_stay_ready_with_a_separate_cause() {
    let plan = plan(
        &user(r#"{"disabled_tools": [], "backup": {"enabled": false}, "inspect": {"enabled": false}, "bash": {"enabled": false}}"#),
        None,
    );
    for (id, cause) in [
        ("aft_safety", "backup_disabled"),
        ("aft_inspect", "inspect_disabled"),
        ("bash", "bash_disabled"),
        ("bash_kill", "bash_disabled"),
    ] {
        assert_eq!(
            matrix(&plan, id),
            json!({"configured": true, "source": "config", "effective": "ready", "available": false, "reason": "configured", "unavailable_reason": cause}),
            "{id}"
        );
    }
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

    assert_eq!(opencode.feature("aft_zoom").unwrap().effective, Effective::Off);
    assert_eq!(pi.feature("aft_zoom").unwrap().effective, Effective::Ready);
    assert_eq!(none.feature("aft_zoom").unwrap().effective, Effective::Ready);
    assert_eq!(pi.feature("indexes.callgraph").unwrap().effective, Effective::Off);
    assert_eq!(pi.feature("indexes.callgraph").unwrap().reason, Some(REASON_CONFIGURED));
    assert_eq!(opencode.feature("indexes.callgraph").unwrap().effective, Effective::Unavailable);
    assert_eq!(omp, pi, "omp resolves the pi harness block");

    // Inside the project: project disables and index offs apply; protected
    // slots and project re-enables are ignored.
    assert_eq!(project.feature("aft_outline").unwrap().effective, Effective::Off);
    assert_eq!(none.feature("aft_outline").unwrap().effective, Effective::Ready);
    assert_eq!(
        matrix(&project, "read"),
        json!({"configured": true, "source": "config", "effective": "ready", "available": true, "reason": "configured", "unavailable_reason": null})
    );
    assert_eq!(project.feature("indexes.trigram").unwrap().effective, Effective::Off);
    assert_eq!(project.feature("indexes.trigram").unwrap().reason, Some(REASON_CONFIGURED));

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
    let inputs = with_project(ConfigInputs::default(), r#"{"disabled_tools": ["aft_zoom"]}"#);
    let plan = plan(&inputs, None);
    assert_eq!(matrix(&plan, "aft_zoom")["reason"], json!("configured"));
    assert_eq!(matrix(&plan, "aft_zoom")["source"], json!("default"));
    assert_eq!(matrix(&plan, "aft_outline")["reason"], json!("default"));
}

#[test]
fn rejected_configuration_produces_no_plan() {
    let errors = derive_plan(
        &user(r#"{"gh_read": {"enabled": true}}"#),
        None,
        &NoRuntimeObservation,
        PolicyPhase::Window,
    )
    .unwrap_err();
    assert_eq!(errors, vec!["removed_config_key:gh_read:use:github.read"]);
    let errors = derive_plan(
        &user(r#"{"search_index": false}"#),
        None,
        &NoRuntimeObservation,
        PolicyPhase::Rejecting,
    )
    .unwrap_err();
    assert_eq!(errors, vec!["removed_config_key:search_index:use:indexes.trigram"]);
    assert!(derive_plan(&user("{ nope"), None, &NoRuntimeObservation, PolicyPhase::Window).is_err());
}

#[test]
fn in_window_legacy_inputs_count_as_configured_choices() {
    let plan = plan(&user(r#"{"tool_surface": "all", "semantic_search": false}"#), None);
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
    assert_eq!(parse_answers(r#"{"selections": {}}"#).unwrap_err(), UNSUPPORTED_PLAN_VERSION);
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
    assert_eq!(value["indexes"], json!({"semantic": false, "trigram": true, "callgraph": true}));
    assert_eq!(value["github"], json!({"read": true, "write": false}));
    assert_eq!(value["disabled_tools"], json!(["aft_delete", "aft_move"]));
}

/// Interactive/yes saving of a write-only choice writes `github.write:true`
/// and leaves `github.read` absent: write implies read at resolution time.
#[test]
fn write_only_github_leaves_read_absent() {
    let (value, _) = written(Some(r#"{"github": {"write": true}}"#), SetupSelections::Yes);
    let github: BTreeSet<&str> = value["github"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(github, BTreeSet::from(["write"]));
    assert_eq!(value["github"]["write"], json!(true));

    let (value, _) = written(None, answers(&[("github.write", true)]));
    assert_eq!(value["github"], json!({"write": true}));

    // An independently explicit read choice survives unless edited.
    let (value, _) = written(Some(r#"{"github": {"read": false, "write": true}}"#), SetupSelections::Yes);
    assert_eq!(value["github"], json!({"read": false, "write": true}));
    let resolved = plan(&user(&serde_json::to_string(&value).unwrap()), None);
    assert_eq!(resolved.feature("github.read").unwrap().effective, Effective::Ready);
}

#[test]
fn partial_answers_preserve_omitted_base_fields() {
    let existing = r#"{"github": {"write": true}, "harnesses": {"pi": {"disabled_tools": ["aft_zoom"]}}}"#;
    let (value, _) = written(Some(existing), answers(&[("aft_move", true)]));
    assert_eq!(value["disabled_tools"], json!(["aft_delete"]));
    assert!(value.get("indexes").is_none(), "omitted indexes stay omitted");
    assert_eq!(value["github"], json!({"write": true}));
    assert_eq!(value["harnesses"], json!({"pi": {"disabled_tools": ["aft_zoom"]}}));
}

#[test]
fn empty_desired_disables_write_a_literal_empty_list() {
    let (value, write) = written(None, answers(&[("aft_move", true), ("aft_delete", true)]));
    assert_eq!(value["disabled_tools"], json!([]));
    assert!(write.text.contains("\"disabled_tools\": []"));
    // `[]` and absence resolve differently: absence restores the default disables.
    let explicit = plan(&user(&write.text), None);
    assert_eq!(explicit.feature("aft_move").unwrap().effective, Effective::Ready);
    let absent = plan(&user("{}"), None);
    assert_eq!(absent.feature("aft_move").unwrap().effective, Effective::Off);
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
        assert!(list.contains(&"aft_future_tool") && list.contains(&"typo_name"), "{list:?}");
        assert_eq!(write.unknown_disabled_tools, vec!["aft_future_tool", "typo_name"]);
    }
    let outcome = derive_plan(&user(existing), None, &NoRuntimeObservation, PolicyPhase::Window).unwrap();
    assert_eq!(outcome.unknown_disabled_tools, vec!["aft_future_tool", "typo_name"]);
    let warning = unknown_disabled_warning(&outcome.unknown_disabled_tools).unwrap();
    assert!(warning.starts_with("unknown_disabled_tools:") && warning.ends_with("aft_future_tool, typo_name"));
}

#[test]
fn historical_aliases_are_written_canonically() {
    let (value, write) = written(Some(r#"{"disabled_tools": ["aft_glob", "aft_move"]}"#), SetupSelections::Yes);
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
    let (value, _) = written_with(Some(r#"{"indexes": {"semantic": true}}"#), SetupSelections::Yes, &unsupported);
    assert_eq!(value["indexes"]["semantic"], json!(true));
}

#[test]
fn setup_writes_only_the_base_and_keeps_comments_and_unrelated_keys() {
    let existing = "{\n  // why hashline\n  \"edit_mode\": \"hashline\",\n  /* block */\n  \"harnesses\": {\"opencode\": {\"indexes\": {\"semantic\": false}}},\n}\n";
    let (value, write) = written(Some(existing), answers(&[("indexes.semantic", true)]));
    assert!(write.text.contains("// why hashline") && write.text.contains("/* block */"));
    assert_eq!(value["indexes"], json!({"semantic": true}));
    assert_eq!(value["harnesses"], json!({"opencode": {"indexes": {"semantic": false}}}));
}
