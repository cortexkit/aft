//! Subconscious's managed settings schema, projected from the editor schema.

use crate::{config_live::restart_required, config_resolve::schema::editor_schema};
use serde_json::{json, Value};

mod defaults;
mod harness;
mod layers;
mod titles;

#[doc(hidden)]
pub fn settings_restart_required(path: &str) -> bool {
    layers::user_only(path) && restart_required(path)
}

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
    annotate_cache_dir(&mut schema["properties"]["storage"]["properties"]["cache_dir"]);
    declare(&mut schema, "", Some(&defaults::values()), "", false);
    // The editor schema's public URL means nothing to Subconscious, which
    // refuses `$id` in a declaration.
    schema
        .as_object_mut()
        .expect("settings schema")
        .remove("$id");
    setup_sections(&mut schema["properties"]);
    schema
}

fn annotate_cache_dir(cache_dir: &mut Value) {
    // The settings editor edits a tagged choice as one JSON value.
    cache_dir["x-ck-kind"] = json!("json");
    for branch in cache_dir["oneOf"]
        .as_array_mut()
        .expect("cache directory choices")
    {
        let kind = branch["properties"]["kind"]["const"]
            .as_str()
            .unwrap()
            .to_owned();
        let (description, kind_description) = match Some(kind.as_str()) {
            Some("default") => (
                "AFT's default place for disk caches.",
                "Select the default cache behavior.",
            ),
            Some("main_disk") => (
                "Keep disk caches inside AFT's storage folder.",
                "Select the storage root for disk caches.",
            ),
            Some("dir") => (
                "Keep disk caches in a folder you choose, for example on another drive.",
                "Select an explicit cache parent directory.",
            ),
            other => panic!("unknown cache directory choice: {other:?}"),
        };
        branch["title"] = json!(titles::title(&format!("storage.cache_dir: {kind}"))
            .expect("cache directory choice title"));
        branch["description"] = json!(description);
        branch["properties"]["kind"]["description"] = json!(kind_description);
        if let Some(path) = branch["properties"].get_mut("path") {
            path["title"] = json!(titles::title("storage.cache_dir: dir.path").unwrap());
            path["description"] =
                json!("Absolute path of an existing folder. ~ means your home folder.");
        }
    }
}

fn setup_sections(fields: &mut Value) {
    for (feature, order, recommended) in [
        (
            "github",
            10,
            json!({"/github/read": true, "/github/shim": false}),
        ),
        ("remote_exec", 20, json!({"/remote_exec/enabled": true})),
        // Offer backend selection without recommending a backend. Disabling
        // meaning search is a separate setting under `indexes`.
        ("semantic", 30, json!({})),
    ] {
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
    if let Some(title) = titles::title(path) {
        node.insert("title".into(), json!(title));
    }
    let positional_default = node.remove("default");
    let default = if harness {
        None
    } else if path.starts_with("shell.env_provider.") {
        // These member defaults apply only when a provider is selected.
        default.or(positional_default.as_ref())
    } else {
        default
    };
    let mut description = node
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_else(|| leaf_description(path, parent_description))
        .to_owned();
    if layers::next_session(path) {
        description.push(' ');
        description.push_str(layers::NEXT_SESSION);
        node.insert("description".into(), json!(description));
        if let Some(text) = node.get_mut("markdownDescription") {
            if let Some(markdown) = text.as_str() {
                *text = json!(format!("{markdown} {}", layers::NEXT_SESSION));
            }
        }
    }
    layers::set(node, path);
    node.insert(
        "x-ck-apply".into(),
        json!(if settings_restart_required(path) {
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
    if node.get("x-ck-kind").and_then(Value::as_str) == Some("json") {
        // The JSON validator checks these children as assertions on one value,
        // not as settings. Keep additionalProperties to reject unknown fields.
        return;
    }
    // Subconscious refuses keywords it does not enforce on ordinary settings.
    // The schema generator adds `format` to integers ("uint64" and so on);
    // every unsigned one also carries `minimum: 0`, so nothing is lost.
    node.remove("format");
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
