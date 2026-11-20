//! Luau module resolution over the caller's immutable resolver snapshot.

use super::{join::dispatch::dependency, FileCallData, ResolverIndex};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

fn normalize(base: &str, path: &str) -> Option<String> {
    if path.starts_with('/') || path.contains(['\\', ':', '\0']) {
        return None;
    }
    let mut parts = base
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn probe(dependencies: &mut BTreeSet<String>, path: &str) {
    // Record both the path and its resolver-file key so both dependency readers
    // invalidate the caller. Missing candidates matter when a file is created.
    dependencies.insert(path.to_string());
    dependencies.insert(dependency::file(path));
}

pub(super) fn module<I: ResolverIndex>(
    index: &I,
    caller: &str,
    module: &str,
    dependencies: &mut BTreeSet<String>,
) -> Option<String> {
    let directory = parent(caller);
    let mut ancestors = Vec::new();
    let mut current = directory;
    loop {
        ancestors.push(current);
        if current.is_empty() {
            break;
        }
        current = parent(current);
    }
    let mut aliases = HashMap::new();
    let reads_aliases =
        module.starts_with('@') && module != "@self" && !module.starts_with("@self/");
    for directory in ancestors.into_iter().rev() {
        let path = if directory.is_empty() {
            ".luaurc".to_string()
        } else {
            format!("{directory}/.luaurc")
        };
        probe(dependencies, &path);
        // Relative, script and @self requires do not read alias values. Keep
        // their path probes, but do not invalidate them for an alias edit.
        if !reads_aliases {
            continue;
        }
        let Some(bytes) = index.luau_config(&path) else {
            continue;
        };
        let Some(config) = serde_json::from_slice::<serde_json::Value>(&bytes).ok() else {
            continue;
        };
        if let Some(entries) = config.get("aliases").and_then(|v| v.as_object()) {
            for (name, value) in entries {
                if let Some(value) = value.as_str() {
                    // Retain an invalid nearer definition as unresolved, rather
                    // than silently selecting the overridden ancestor alias.
                    aliases.insert(name.clone(), normalize(directory, value));
                }
            }
        }
    }
    let is_init = caller.rsplit('/').next() == Some("init.luau");
    let own_module = if is_init {
        directory
    } else {
        caller.strip_suffix(".luau")?
    };
    let base = if is_init {
        parent(directory)
    } else {
        directory
    };
    let path = if let Some(steps) = module.strip_prefix(crate::luau::SCRIPT_PREFIX) {
        let steps: Vec<Option<String>> = serde_json::from_str(steps).ok()?;
        let mut path = own_module.to_string();
        for step in steps {
            match step {
                None => {
                    if path.is_empty() {
                        return None;
                    }
                    path = parent(&path).to_string();
                }
                Some(child) => {
                    if child.is_empty()
                        || child.contains(['/', '\\'])
                        || matches!(child.as_str(), "." | "..")
                    {
                        return None;
                    }
                    path = normalize(&path, &child)?;
                }
            }
        }
        path
    } else if module == "@self" || module.starts_with("@self/") {
        normalize(
            own_module,
            module
                .strip_prefix("@self")
                .unwrap_or_default()
                .trim_start_matches('/'),
        )?
    } else if let Some(alias) = module.strip_prefix('@') {
        let (name, suffix) = alias.split_once('/').unwrap_or((alias, ""));
        normalize(aliases.get(name)?.as_deref()?, suffix)?
    } else if module.starts_with("./") || module.starts_with("../") {
        normalize(base, module)?
    } else {
        return None;
    };
    if path.split('/').any(|p| p == "Packages")
        || path.ends_with(".lua")
        || path.ends_with(".server.luau")
        || path.ends_with(".client.luau")
    {
        return None;
    }
    let stem = path.strip_suffix(".luau").unwrap_or(&path);
    if stem.ends_with(".server") || stem.ends_with(".client") {
        return None;
    }
    let file = format!("{stem}.luau");
    let init = if stem.is_empty() {
        "init.luau".to_string()
    } else {
        format!("{stem}/init.luau")
    };
    probe(dependencies, &file);
    probe(dependencies, &init);
    let present_file = index.contains_file(&file);
    let present_init = index.contains_file(&init);
    match (present_file, present_init) {
        (true, false) => Some(file),
        (false, true) => Some(init),
        _ => None,
    }
}

pub(super) fn call<I: ResolverIndex>(
    index: &I,
    caller: &str,
    full: &str,
    short: &str,
    data: &FileCallData,
    dependencies: &mut BTreeSet<String>,
) -> Option<(String, String, String)> {
    if let Some((binding, member)) = full.split_once(['.', ':']) {
        if member.is_empty() || member.contains(['.', ':', '[', '(']) {
            return None;
        }
        let import = data
            .import_block
            .imports
            .iter()
            .find(|import| import.default_import.as_deref() == Some(binding))?;
        let file = module(index, caller, &import.module_path, dependencies)?;
        // Only an explicit returned-table alias can link a Luau module member.
        // In particular, never try a bare-name lookup in the target module.
        let symbol = index.export_alias(&file, member)?;
        let node = index.node_for_symbol(&file, &symbol)?;
        if !index.node_is_callable(&file, &node) {
            return None;
        }
        Some(("resolved".into(), file, symbol))
    } else {
        super::resolve_local_target(index, caller, full, short, data)
    }
}

pub(super) fn config(index: &super::ProjectIndex<'_>, file: &str) -> Option<Arc<[u8]>> {
    index.facts.config_bytes(file.as_bytes())
}
