//! Subconscious's managed settings schema, projected from the editor schema.

use crate::{config_live::restart_required, config_resolve::schema::editor_schema};
use serde_json::{json, Value};

mod defaults;
mod harness;

const MAP_KEY: &str = "^.+$";

/// Generate the declaration without registering it or reading a settings file.
pub fn settings_declaration() -> Value {
    let mut schema = editor_schema();
    let fields = schema["properties"].as_object_mut().expect("raw settings");
    let overrides = harness::fields(fields);
    let harnesses = fields.get_mut("harnesses").expect("harness map");
    harnesses["additionalProperties"] = json!({"type": "object", "properties": overrides});
    for key in ["root_ttl_minutes", "checkout_hours"] {
        let idle = schema["properties"]["idle"]["properties"][key]
            .as_object_mut()
            .expect("idle setting schema");
        idle.remove("not");
        idle.insert("type".into(), json!("integer"));
    }
    declare(&mut schema, "", Some(&defaults::values()), "", false);
    setup_sections(&mut schema["properties"]);
    schema
}

fn setup_sections(fields: &mut Value) {
    for (feature, title, order, recommended) in [
        (
            "github",
            "GitHub",
            10,
            json!({"/github/read": true, "/github/shim": false}),
        ),
        (
            "remote_exec",
            "Remote runs",
            20,
            json!({"/remote_exec/enabled": true}),
        ),
        // Offer backend selection without recommending a backend. Disabling
        // meaning search is a separate setting under `indexes`.
        ("semantic", "Meaning search", 30, json!({})),
    ] {
        fields[feature]["title"] = json!(title);
        fields[feature]["x-ck-setup"] =
            json!({"feature": feature, "order": order, "recommended": recommended});
    }
}

fn declare(
    schema: &mut Value,
    path: &str,
    default: Option<&Value>,
    parent_description: &str,
    harness: bool,
) {
    let node = schema.as_object_mut().expect("settings schema node");
    let positional_default = node.remove("default");
    let default = if harness {
        None
    } else if path.starts_with("shell.env_provider.") {
        // These member defaults apply only when a provider is selected.
        default.or(positional_default.as_ref())
    } else {
        default
    };
    let description = node
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_else(|| leaf_description(path, parent_description))
        .to_owned();
    node.insert("x-ck-layers".into(), json!(["user"]));
    node.insert(
        "x-ck-apply".into(),
        json!(if restart_required(path) {
            "module_restart"
        } else {
            "live"
        }),
    );
    if let Some(default) = default.filter(|v| !v.is_null()) {
        // Defaults for fixed objects come from their leaves; maps, arrays and
        // tagged choices take their default as a whole value.
        if !node.contains_key("properties") {
            node.insert("default".into(), default.clone());
        }
    } else if !path.is_empty() {
        node.insert("x-ck-required".into(), json!(false));
    }
    let structural = node.get("type") == Some(&json!("object"));
    if structural {
        node.insert("unevaluatedProperties".into(), json!(false));
        let additional = node.remove("additionalProperties");
        // The resolver only warns about unknown index entry fields when it
        // reads a file directly. Subconscious accepts only path and indexes.
        if let Some(values) = additional.filter(|v| v.is_object() && path != "index.roots.*") {
            node.insert("patternProperties".into(), json!({MAP_KEY: values}));
        }
    } else {
        node.entry("description").or_insert(json!(description));
    }
    if let Some(children) = node.get_mut("properties").and_then(Value::as_object_mut) {
        for (key, child) in children {
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            declare(
                child,
                &child_path,
                default.and_then(|v| v.get(key)),
                &description,
                harness,
            );
        }
    }
    if let Some(child) = node.get_mut("items") {
        declare(child, &format!("{path}.*"), None, &description, harness);
    }
    if let Some(patterns) = node
        .get_mut("patternProperties")
        .and_then(Value::as_object_mut)
    {
        let (child_path, is_harness) = if path == "harnesses" {
            (String::new(), true)
        } else {
            (format!("{path}.*"), harness)
        };
        declare(
            &mut patterns[MAP_KEY],
            &child_path,
            None,
            &description,
            is_harness,
        );
    }
    if let Some(branches) = node.get_mut("oneOf").and_then(Value::as_array_mut) {
        for branch in branches {
            declare(branch, path, None, &description, harness);
        }
    }
}

fn leaf_description<'a>(path: &str, parent: &'a str) -> &'a str {
    match path {
        "$schema" => "Editor schema URL.",
        "shell.env_provider.kind" => "`mise` (runs `mise env --json`), `direnv` (runs `direnv export json`; not supported on Windows) or `custom`.",
        "shell.env_provider.timeout_ms" => "100 to 10000, default 2000. The provider also has to fit in the time a bash call allows for starting its command: when less than `timeout_ms` of that is left, the provider gets what is left minus one second.",
        "sandbox.profile.worker" => "Stamped Alfonso workers use `worker`; heads, unstamped routes, standalone invocations and default sessions use `head`.",
        "sandbox.profile.head" => "Stamped Alfonso workers use `worker`; heads, unstamped routes, standalone invocations and default sessions use `head`.",
        _ => parent,
    }
}

#[cfg(test)]
mod tests;
