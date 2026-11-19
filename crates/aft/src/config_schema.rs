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
    make_provider_union(&mut schema["properties"]["shell"]["properties"]["env_provider"]);
    // The harness projection also permits the provider, but never its defaults.
    make_provider_union(
        &mut schema["properties"]["harnesses"]["additionalProperties"]["properties"]["shell"]
            ["properties"]["env_provider"],
    );
    declare(&mut schema, "", Some(&defaults::values()), "", false);
    schema
}

// An ordinary properties object would synthesize an invalid, kind-less provider
// in the effective view even when the user omitted it. Each existing kind is
// already a discriminator, so keep the same file shape as a tagged union.
fn make_provider_union(schema: &mut Value) {
    let node = schema.as_object_mut().expect("shell provider schema");
    let mut branch = Value::Object(std::mem::take(node));
    schema["description"] = branch
        .as_object_mut()
        .unwrap()
        .remove("description")
        .expect("shell provider description");
    let kinds = branch["properties"]["kind"]
        .as_object_mut()
        .unwrap()
        .remove("enum")
        .expect("shell provider kinds");
    schema["oneOf"] = Value::Array(
        kinds
            .as_array()
            .unwrap()
            .iter()
            .map(|kind| {
                branch["properties"]["kind"]["const"] = kind.clone();
                branch.clone()
            })
            .collect(),
    );
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
    // Keep the resolver's warnings for unknown root-entry properties and
    // invalid idle values rather than rejecting those values at the gate.
    if matches!(
        path,
        "index.roots.*" | "idle.root_ttl_minutes" | "idle.checkout_hours"
    ) {
        node.insert("x-ck-kind".into(), json!("json"));
        node.entry("description").or_insert(json!(description));
        return;
    }
    if structural {
        node.insert("unevaluatedProperties".into(), json!(false));
        if let Some(values) = node.remove("additionalProperties").filter(Value::is_object) {
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
