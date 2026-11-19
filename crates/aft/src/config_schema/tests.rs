use super::*;
use std::collections::BTreeSet;
use subc_config::{Layer, LayerValue, RefusalCode, Schema, ValidationContext};

fn validated() -> Schema {
    Schema::new(settings_declaration()).expect("Subconscious accepts the declaration")
}

fn user_fixture(value: Value) -> crate::config_resolve::ConfigTier {
    crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "settings fixture".into(),
        doc: value.to_string(),
    }
}

#[test]
fn config_settings_editor_output_is_unchanged() {
    let mut bytes = serde_json::to_vec_pretty(&editor_schema()).unwrap();
    bytes.push(b'\n');
    assert!(bytes == include_bytes!("../../../../assets/aft.schema.json"));
}

fn annotations(node: &Value, path: &str, found: &mut BTreeSet<(String, String)>) {
    for keyword in ["description", "markdownDescription"] {
        if let Some(text) = node.get(keyword).and_then(Value::as_str) {
            found.insert((format!("{path}/{keyword}"), text.to_owned()));
        }
    }
    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            annotations(child, &format!("{path}/{key}"), found);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(child) = node.get(key) {
            annotations(child, &format!("{path}/*"), found);
        }
    }
    if let Some(child) = node.pointer("/patternProperties/^.+$") {
        annotations(child, &format!("{path}/*"), found);
    }
    if let Some(branches) = node.get("oneOf").and_then(Value::as_array) {
        for branch in branches {
            annotations(branch, path, found);
        }
    }
}

#[test]
fn config_settings_preserves_existing_descriptions_word_for_word() {
    let editor = editor_schema();
    let declaration = settings_declaration();
    let mut expected = BTreeSet::new();
    let mut actual = BTreeSet::new();
    annotations(&editor, "", &mut expected);
    annotations(&declaration, "", &mut actual);
    assert!(expected.len() >= 200);
    for (path, text) in expected {
        // The editor retains every suggestion; only managed per-harness
        // settings are repeated in the declaration.
        if let Some(harness_path) = path.strip_prefix("/harnesses/*/") {
            let block = &declaration["properties"]["harnesses"]["patternProperties"][MAP_KEY];
            let nodes: Vec<_> = harness_path.split('/').collect();
            if !nodes[..nodes.len() - 1]
                .iter()
                .try_fold(block, |node, key| {
                    if *key == "*" {
                        node.get("items")
                            .or_else(|| node.pointer("/patternProperties/^.+$"))
                    } else {
                        node.get("properties")?.get(key)
                    }
                })
                .is_some()
                && !harness_path.starts_with("shell/env_provider/")
            {
                continue;
            }
        }
        assert!(
            actual.contains(&(path.clone(), text)),
            "description drift at {path}"
        );
    }
}

#[test]
fn config_settings_subconscious_defaults_match_ungated_resolution() {
    let schema = validated();
    let context = ValidationContext::default();
    let effective = schema.review_tuple(&[], &context).unwrap().effective;
    assert!(effective.get("memory").is_none());
    assert!(effective.pointer("/shell/env_provider").is_none());
    assert!(effective.get("harnesses").is_none());
    assert_eq!(
        effective["storage"]["cache_dir"],
        json!({"kind": "default"})
    );
    let tier = user_fixture(effective);
    let resolved = crate::config_resolve::resolve_config(&[tier]);
    assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
    let empty = crate::config_resolve::resolve_config(&[]);
    assert_eq!(json!(resolved.config), json!(empty.config));
}

#[test]
fn config_settings_optional_fields_remain_absent_and_maps_are_typed() {
    let schema = validated();
    let context = ValidationContext::default();
    let value = json!({"lsp": {"servers": {"rust-analyzer": {"args": ["--example"]}}}});
    let effective = schema
        .review_tuple(&[LayerValue::new(Layer::User, value)], &context)
        .unwrap()
        .effective;
    assert_eq!(
        effective["lsp"]["servers"]["rust-analyzer"],
        json!({"args": ["--example"]})
    );
    let entry = &schema.normalized()["properties"]["lsp"]["properties"]["servers"]
        ["patternProperties"][MAP_KEY]["properties"];
    for (key, field) in entry.as_object().unwrap() {
        assert!(field.get("default").is_none(), "{key}");
        assert_eq!(field["x-ck-required"], false, "{key}");
        assert_eq!(field["x-ck-apply"], "module_restart", "{key}");
    }
    for value in [
        json!({"formatter":{"rust":true}}),
        json!({"formatter":{"":"rustfmt"}}),
    ] {
        assert!(schema
            .validate_layer(Layer::User, &value, &context)
            .is_err());
    }
    assert!(schema
        .validate_layer(Layer::Project, &json!({"format_on_edit":true}), &context)
        .is_err());
}

#[test]
fn config_settings_harness_overrides_are_closed_and_inherit() {
    let schema = validated();
    fn has_default(node: &Value) -> bool {
        match node {
            Value::Object(map) => map.contains_key("default") || map.values().any(has_default),
            Value::Array(items) => items.iter().any(has_default),
            _ => false,
        }
    }
    let block = &schema.normalized()["properties"]["harnesses"]["patternProperties"][MAP_KEY];
    assert!(!has_default(block));
    let context = ValidationContext::default();
    let value =
        json!({"bash": {"rewrite": true}, "harnesses": {"opencode": {"bash": {"enabled": false}}}});
    let effective = schema
        .review_tuple(&[LayerValue::new(Layer::User, value)], &context)
        .unwrap()
        .effective;
    assert_eq!(
        effective["harnesses"]["opencode"]["bash"],
        json!({"enabled": false})
    );
    let bound = crate::config_resolve::resolve_config_for_harness(
        &[user_fixture(effective)],
        Some(&crate::harness::Harness::Opencode),
    );
    assert!(bound.errors.is_empty());
    assert!(!bound.config.bash.enabled);
    assert!(bound.config.experimental_bash_rewrite);
    for excluded in [
        "memory",
        "storage",
        "index",
        "indexes",
        "remote_exec",
        "subc",
        "backup",
    ] {
        let value = json!({"harnesses":{"opencode":{(excluded):{}}}});
        let issue = schema
            .validate_layer(Layer::User, &value, &context)
            .unwrap_err();
        assert_eq!(issue.path, format!("/harnesses/opencode/{excluded}"));
    }
    for (section, excluded) in [
        ("lsp", "servers"),
        ("sandbox", "profile"),
        ("semantic", "backend"),
    ] {
        let value = json!({"harnesses":{"opencode":{(section):{(excluded):{}}}}});
        let issue = schema
            .validate_layer(Layer::User, &value, &context)
            .unwrap_err();
        assert_eq!(
            issue.path,
            format!("/harnesses/opencode/{section}/{excluded}")
        );
    }
}

#[test]
fn config_settings_accepts_mac_user_shape_except_for_subc_0_1_3_env_singleton() {
    let schema = validated();
    let context = ValidationContext::default();
    // Only the paths and types come from the live file; these values are fabricated.
    let mut value = json!({
        "gh_shim": {"binary_path": "example/path"}, "git": {"co_author": "off"},
        "restrict_to_project_root": false,
        "semantic": {"backend": "fastembed", "model": "example-model", "timeout_ms": 1234},
        "harnesses": {"opencode": {"bash": {"env": {"EXAMPLE_VARIABLE": "example"}}}},
        "bash": {"subagent_background": true, "rewrite": false, "compress": false,
                 "background": false, "disclaim_privacy": false},
        "remote_exec": {"enabled": false, "default_demand": "example"},
        "subc": {"connection_file": "example/path"}, "lsp": {"idle_minutes": 42},
        "search": {"rerank": {"backend": "off", "model": "example-model", "top_n": 12}},
        "github": {"read": false, "write": false}, "disabled_tools": [],
        "indexes": {"trigram": false, "semantic": false}
    });
    // subc-config 0.1.3 src/schema.rs:844–846 mistakes a sole `env` property
    // for a value reference even when the object declares that property.
    let refusal = schema
        .validate_layer(Layer::User, &value, &context)
        .expect_err(
            "remove this expected refusal once Subconscious accepts a declared env property",
        );
    assert_eq!(refusal.code, RefusalCode::ValueInvalid);
    assert_eq!(refusal.path, "/harnesses/opencode/bash");
    value["harnesses"]["opencode"]["bash"]["rewrite"] = json!(false);
    schema
        .validate_layer(Layer::User, &value, &context)
        .unwrap();
}

#[test]
fn config_settings_shell_provider_keeps_its_existing_tagged_file_shapes() {
    let schema = validated();
    let context = ValidationContext::default();
    for provider in [
        json!({"kind":"mise"}),
        json!({"kind":"direnv"}),
        json!({"kind":"custom", "command":["example"], "watch":["tool.toml"], "timeout_ms":500}),
    ] {
        let value = json!({"shell": {"env_provider": provider}});
        let effective = schema
            .review_tuple(&[LayerValue::new(Layer::User, value.clone())], &context)
            .unwrap()
            .effective;
        assert_eq!(
            effective.pointer("/shell/env_provider"),
            value.pointer("/shell/env_provider")
        );
    }
    for text in [
        include_str!(
            "../../tests/fixtures/config_parity/shell_env_provider_user_opencode/user.jsonc"
        ),
        include_str!(
            "../../tests/fixtures/config_parity/shell_env_provider_custom_opencode/user.jsonc"
        ),
        include_str!(
            "../../tests/fixtures/config_parity/shell_env_provider_harness_watch_pi/user.jsonc"
        ),
    ] {
        let value: Value = serde_json::from_str(text).unwrap();
        let effective = schema
            .review_tuple(&[LayerValue::new(Layer::User, value.clone())], &context)
            .unwrap()
            .effective;
        for path in ["/shell/env_provider", "/harnesses/pi/shell/env_provider"] {
            assert_eq!(effective.pointer(path), value.pointer(path));
        }
    }
}
