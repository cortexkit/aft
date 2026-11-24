use super::*;
use cortexkit_role_tool_provider::catalog::schema_digest;

fn read_tool() -> &'static CatalogTool {
    &served_tools(CatalogPreset::Worker)
        .iter()
        .find(|tool| tool.catalog.name == "read")
        .unwrap()
        .catalog
}

fn admit(call: &ToolCallRequest) -> Result<CallerRole, ErrorBody> {
    super::super::admit(call, false, &[], true, "session", true)
}

#[test]
fn catalog_publication_switch_controls_the_wire_version_and_digest() {
    let render = || super::super::catalog(json!({"preset":"reader"}), &[], false).unwrap();
    let default = render();
    let _legacy = legacy_catalog();
    let legacy = render();
    let published = default["tools"].as_array().unwrap();
    assert!(!published.is_empty());
    for tool in published {
        if PUBLISH_TOOL_VERSIONS {
            assert_eq!(tool["version"], "1.0.0");
        } else {
            assert!(tool.get("version").is_none());
        }
    }
    assert_eq!(
        default["catalog_digest"] != legacy["catalog_digest"],
        PUBLISH_TOOL_VERSIONS
    );
}

#[test]
fn tp2_same_major_uses_the_current_schema_and_existing_argument_refusals() {
    let current = read_tool();
    let mut newer = current.input_schema.clone();
    newer["properties"]["futureArgument"] = json!({"type":"boolean"});
    let digest = schema_digest(&newer).unwrap();
    assert_ne!(digest, current.schema_digest);
    let pin = ToolMajorPin::new("read", digest, 1).encode().unwrap();
    let mut call = ToolCallRequest::new("read", json!({"path":"src/main.rs"}));
    call.schema_pin = Some(pin);
    assert_eq!(admit(&call).unwrap(), CallerRole::Head);

    call.arguments["futureArgument"] = json!(true);
    let error = admit(&call).unwrap_err();
    assert_eq!(error.code, "invalid_request");
    assert_eq!(error.message, "argument is absent from the served schema");
    assert_eq!(error.detail, Some(json!({"field":"futureArgument"})));
}

#[test]
fn tp2_major_refusal_has_the_crates_message_and_exact_pins() {
    let current = read_tool();
    let pin = ToolMajorPin::new("read", &current.schema_digest, 2)
        .encode()
        .unwrap();
    let mut call = ToolCallRequest::new("read", json!({"path":"src/main.rs"}));
    call.schema_pin = Some(pin.clone());
    let error = admit(&call).unwrap_err();
    assert_eq!(error.code, "tool_major_changed");
    assert_eq!(error.message, "this tool changed incompatibly");
    assert_eq!(
        error.detail,
        Some(
            json!({"tool":"read", "expected":pin, "current":current.schema_pin().encode().unwrap()})
        )
    );
}

#[test]
fn an_aliased_major_refusal_names_the_wire_tool_and_pins() {
    let tool = &served_tools(CatalogPreset::Head)
        .iter()
        .find(|tool| tool.catalog.name == "outline")
        .unwrap()
        .catalog;
    let sent = ToolMajorPin::new("aft_outline", &tool.schema_digest, 2)
        .encode()
        .unwrap();
    let mut call = ToolCallRequest::new("aft_outline", json!({"target":"src/main.rs"}));
    call.schema_pin = Some(sent.clone());
    let original = super::super::route_head_call_name(&mut call, false);
    assert_eq!(original.as_deref(), Some(sent.as_str()));
    let error = routed::wire_refusal(admit(&call).unwrap_err(), original.as_deref());
    let expected_current = ToolMajorPin::new("aft_outline", &tool.schema_digest, 1)
        .encode()
        .unwrap();
    assert_eq!(
        error.detail,
        Some(json!({"tool":"aft_outline", "expected":sent, "current":expected_current}))
    );
    assert_eq!(record_pin(original, call.schema_pin).unwrap(), sent);
}

#[test]
fn unmapped_tp1_preserves_both_legacy_refusal_bodies() {
    let tool = &served_tools(CatalogPreset::Head)
        .iter()
        .find(|tool| tool.catalog.name == "bash")
        .unwrap()
        .catalog;
    // The old schema refusal used the validation schema, which adds `runon`.
    let remote = &served_tools(CatalogPreset::Worker)
        .iter()
        .find(|tool| tool.catalog.name == "bash")
        .unwrap()
        .with_runon
        .as_ref()
        .unwrap()
        .schema_digest;
    assert_ne!(&tool.schema_digest, remote);
    let unknown_digest = "0".repeat(64);
    for (digest, expected) in [
        (&tool.schema_digest, r#"{"code":"tool_semantics_changed","message":"semantics pin is stale","detail":{"tool":"bash","expected":99,"current":3}}"#.to_owned()),
        (&unknown_digest, format!(r#"{{"code":"tool_schema_changed","message":"schema pin is stale","detail":{{"tool":"bash","expected":"{unknown_digest}","current":"{remote}"}}}}"#)),
    ] {
        let mut call = ToolCallRequest::new("bash", json!({"command":"echo"}));
        call.schema_pin = Some(SchemaPin::new("bash", digest, 99).encode().unwrap());
        let body = serde_json::to_value(admit(&call).unwrap_err()).unwrap();
        assert_eq!(body.to_string(), expected);
    }
}

#[test]
fn every_served_legacy_variant_and_permanent_view_is_in_the_history() {
    let head = &super::super::RUNNER_HEAD_TOOLS;
    let mut names = std::collections::BTreeSet::new();
    for tool in CatalogPreset::ALL
        .into_iter()
        .flat_map(served_tools)
        .chain([&head.hashline_edit, &head.bash_watch])
    {
        for variant in [
            Some(&tool.catalog),
            tool.with_runon.as_ref(),
            tool.with_ref.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            names.insert(variant.name.as_str());
            let pin = SchemaPin::new(&variant.name, &variant.schema_digest, variant.semantics)
                .encode()
                .unwrap();
            assert_eq!(SHIPPED.get(&pin), Some(&1), "{pin}");
        }
    }
    assert_eq!(names, VERSIONS.iter().map(|(name, _)| *name).collect());
    for pin in crate::subc::tool_views::SHIPPED.keys() {
        assert_eq!(SHIPPED.get(pin), Some(&1), "{pin}");
    }
    for key in SHIPPED.keys() {
        assert!(
            names.contains(SchemaPin::parse(key).unwrap().tool.as_str()),
            "{key}"
        );
    }
}
