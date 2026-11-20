use serde_json::{json, Map, Value};

pub(crate) struct Edit {
    pub(crate) path: Vec<String>,
    pub(crate) value: Option<Value>,
    pub(crate) notice: &'static str,
}

impl Edit {
    fn at(path: &[&str], value: Option<Value>, notice: &'static str) -> Self {
        Self {
            path: path.iter().map(|part| (*part).into()).collect(),
            value,
            notice,
        }
    }

    pub(super) fn apply(self, block: &mut Map<String, Value>) {
        let (key, parents) = self.path.split_last().expect("settings edit has a path");
        let mut parent = block;
        for part in parents {
            parent = parent
                .get_mut(part)
                .and_then(Value::as_object_mut)
                .expect("settings edit's parent exists");
        }
        match self.value {
            Some(value) => {
                parent.insert(key.clone(), value);
            }
            None => {
                parent.remove(key);
            }
        }
    }
}

pub(crate) fn cache_dir_replacement(value: &Value) -> Option<(Value, &'static str)> {
    match value {
        Value::Null => Some((
            json!({"kind": "main_disk"}),
            "storage.cache_dir null → storage.cache_dir main_disk",
        )),
        Value::String(path) => Some((
            json!({"kind": "dir", "path": path}),
            "storage.cache_dir string → storage.cache_dir dir",
        )),
        _ => None,
    }
}

pub(crate) fn edits(
    block: &Map<String, Value>,
    tier: super::DocumentTier,
    is_base: bool,
) -> Vec<Edit> {
    let mut edits = Vec::new();
    if let Some(enabled) = block.get("bash").and_then(Value::as_bool) {
        let value = if enabled {
            let mut enabled = json!({"rewrite": true, "compress": true, "background": true});
            // A project boolean used to change only the feature switches if a
            // user set the gate. Rewriting it must not re-enable that gate.
            if tier == super::DocumentTier::User {
                enabled["enabled"] = Value::Bool(true);
            }
            enabled
        } else {
            // Catalog and hashline rendering read these switches even when the runtime gate is off.
            json!({"enabled": false, "rewrite": false, "compress": false, "background": false})
        };
        let notice = if enabled && tier == super::DocumentTier::Project {
            "bash boolean → bash.rewrite, bash.compress and bash.background"
        } else {
            "bash boolean → bash.enabled"
        };
        edits.push(Edit::at(&["bash"], Some(value), notice));
    }
    if let Some(lsp) = block.get("lsp").and_then(Value::as_object) {
        if lsp.get("idle_minutes").and_then(Value::as_str) == Some("never") {
            const NOTICE: &str = "lsp.idle_minutes \"never\" → lsp.idle_shutdown";
            if !lsp.contains_key("idle_shutdown") {
                edits.push(Edit::at(
                    &["lsp", "idle_shutdown"],
                    Some(Value::Bool(false)),
                    NOTICE,
                ));
            }
            edits.push(Edit::at(&["lsp", "idle_minutes"], None, NOTICE));
        }
        if let Some(servers) = lsp.get("servers").and_then(Value::as_object) {
            for (name, server) in servers {
                if let Some(value) = server
                    .get("initialization_options")
                    .filter(|v| !v.is_null() && !v.is_string())
                {
                    edits.push(Edit::at(
                        &["lsp", "servers", name, "initialization_options"],
                        Some(Value::String(value.to_string())),
                        "lsp.servers.<name>.initialization_options → JSON text",
                    ));
                }
            }
        }
    }
    if let Some((value, notice)) = block
        .get("storage")
        .and_then(|storage| storage.get("cache_dir"))
        .filter(|value| !value.is_null() || (is_base && tier == super::DocumentTier::User))
        .and_then(cache_dir_replacement)
    {
        edits.push(Edit::at(&["storage", "cache_dir"], Some(value), notice));
    }
    if let Some(mirrors) = block
        .get("inspect")
        .and_then(|v| v.get("duplicates"))
        .and_then(|v| v.get("expected_mirrors"))
        .and_then(Value::as_array)
    {
        let mut changed = false;
        let current: Vec<_> = mirrors
            .iter()
            .map(|entry| {
                if let Some([Value::String(source), Value::String(mirror)]) =
                    entry.as_array().map(Vec::as_slice)
                {
                    changed = true;
                    json!({"source": source, "mirror": mirror})
                } else {
                    entry.clone()
                }
            })
            .collect();
        if changed {
            edits.push(Edit::at(
                &["inspect", "duplicates", "expected_mirrors"],
                Some(Value::Array(current)),
                "inspect.duplicates.expected_mirrors pairs → source and mirror objects",
            ));
        }
    }
    edits
}

/// Find null after translating legacy spellings. The project-base legacy
/// cache opt-out was accepted but ignored; null in a harness was invalid.
pub(crate) fn null_path(block: &Map<String, Value>) -> Option<String> {
    fn check(value: &Value, path: &mut Vec<String>) -> Option<String> {
        match value {
            Value::Null if !matches!(path.as_slice(), [storage, cache] if storage == "storage" && cache == "cache_dir") => {
                Some(path.join("."))
            }
            Value::Object(map) => map.iter().find_map(|(key, value)| {
                path.push(key.clone());
                let found = check(value, path);
                path.pop();
                found
            }),
            Value::Array(values) => values.iter().enumerate().find_map(|(i, value)| {
                path.push(i.to_string());
                let found = check(value, path);
                path.pop();
                found
            }),
            _ => None,
        }
    }
    block
        .iter()
        .find_map(|(name, value)| check(value, &mut vec![name.clone()]))
}
