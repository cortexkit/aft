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
    for directory in ancestors.into_iter().rev() {
        let path = if directory.is_empty() {
            ".luaurc".to_string()
        } else {
            format!("{directory}/.luaurc")
        };
        probe(dependencies, &path);
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

#[cfg(all(
    test,
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
mod tests {
    use super::*;
    use crate::callgraph_store::{
        facts::{DirEntry, ProjectFacts},
        join::{self, BlobRefKind, CallgraphBlob, DerivedRow, ParseBlob},
        ProjectIndex, WorkspaceCratePrefixCache,
    };
    use std::{collections::BTreeMap, path::Path, rc::Rc};

    struct MemoryFacts(BTreeMap<String, Vec<u8>>);

    impl ProjectFacts for MemoryFacts {
        fn is_file(&self, path: &[u8]) -> bool {
            self.0.contains_key(std::str::from_utf8(path).unwrap())
        }
        fn is_dir(&self, path: &[u8]) -> bool {
            let path = std::str::from_utf8(path).unwrap();
            self.0
                .keys()
                .any(|key| key.starts_with(&format!("{path}/")))
        }
        fn config_bytes(&self, path: &[u8]) -> Option<Arc<[u8]>> {
            self.0
                .get(std::str::from_utf8(path).unwrap())
                .cloned()
                .map(Into::into)
        }
        fn symlink_target(&self, _: &[u8]) -> Option<&[u8]> {
            None
        }
        fn canonical(&self, path: &[u8]) -> Option<Vec<u8>> {
            self.is_file(path).then(|| path.to_vec())
        }
        fn list_dir(&self, _: &[u8]) -> Vec<DirEntry> {
            Vec::new()
        }
    }

    fn resolve(
        caller: &str,
        sources: &[(&str, &str)],
        configs: &[(&str, &str)],
    ) -> (
        ParseBlob,
        BTreeSet<DerivedRow>,
        join::ViewBindingDependencies,
    ) {
        crate::luau::tests::ready();
        let facts = Rc::new(join::ViewBindingFacts::typed(Rc::new(MemoryFacts(
            sources
                .iter()
                .chain(configs)
                .map(|(path, source)| (path.to_string(), source.as_bytes().to_vec()))
                .collect(),
        ))));
        let mut blobs = BTreeMap::new();
        let mut surfaces = HashMap::new();
        let mut bindings = HashMap::new();
        for &(path, source) in sources {
            let language = if path.ends_with(".lua") {
                "lua"
            } else {
                "luau"
            };
            let record = crate::data_layer::content::extract(source.as_bytes(), language).unwrap();
            let CallgraphBlob::Parse(blob) =
                crate::data_layer::content::blob_from_record(&record).unwrap()
            else {
                panic!("parse expected")
            };
            let (surface, binding) = join::bind_file(path, &blob, &facts, None).unwrap();
            surfaces.insert(path.to_string(), surface);
            bindings.insert(path.to_string(), binding);
            blobs.insert(path.to_string(), blob);
        }
        let index = ProjectIndex::from_shared(
            Path::new("/no-such-luau-checkout"),
            Arc::new(surfaces),
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            facts.clone(),
        );
        let blob = blobs.remove(caller).unwrap();
        let mut binding = bindings.remove(caller).unwrap();
        let mut rows = BTreeSet::new();
        join::resolve_file(
            caller,
            &"fixture".to_string(),
            &blob,
            &mut binding,
            &index,
            &facts,
            None,
            &mut rows,
        )
        .unwrap();
        join::finish_binding(&mut binding);
        (blob, rows, binding)
    }

    const TARGET: &str = "return { f = function() end, m = function() end }";

    #[test]
    fn luau_s3_require_forms_resolve_only_indexed_candidates_and_record_all_probes() {
        for (caller, form, expected) in [
            ("a/b.luau", "\"./x\"", Some("a/x.luau")),
            ("a/b.luau", "'./x.luau'", Some("a/x.luau")),
            ("a/b.luau", "\"../x\"", Some("x.luau")),
            ("a/b.luau", "\"@shared/x\"", Some("shared/x.luau")),
            ("a/b.luau", "\"@near/x\"", Some("a/near/x.luau")),
            ("a/b.luau", "script.Parent.x", Some("a/x.luau")),
            ("a/b.luau", "script.Parent[\"x\"]", Some("a/x.luau")),
            (
                "a/b.luau",
                "script.Parent:WaitForChild(\"x\")",
                Some("a/x.luau"),
            ),
            (
                "a/b.luau",
                "script.Parent:FindFirstChild('x')",
                Some("a/x.luau"),
            ),
            ("a/init.luau", "\"./x\"", Some("x.luau")),
            ("a/init.luau", "\"@self/x\"", Some("a/x.luau")),
            ("a/init.luau", "script.x", Some("a/x.luau")),
            ("a/b.luau", "\"./folder\"", Some("a/folder/init.luau")),
            ("a/b.luau", "\"./ambiguous\"", None),
            ("a/b.luau", "\"./old.lua\"", None),
            ("a/b.luau", "script.Parent.old", None),
            ("a/b.luau", "\"./run.server.luau\"", None),
            ("a/b.luau", "script.Parent[\"run.server\"]", None),
            ("a/b.luau", "\"./run.client.luau\"", None),
            ("a/b.luau", "\"../Packages/x\"", None),
            ("a/b.luau", "\"../../escape\"", None),
            ("a/b.luau", "script.Parent.Parent.Parent.x", None),
            ("a/b.luau", "script.Parent[name]", None),
            ("a/b.luau", "script.Parent:WaitForChild(name)", None),
            ("a/b.luau", "game:GetService(\"ReplicatedStorage\").x", None),
            ("a/b.luau", "\"@missing/x\"", None),
        ] {
            let source =
                format!("local M = require({form})\nlocal function caller() M.f() M:m() end");
            let (blob, rows, binding) = resolve(
                caller,
                &[
                    (caller, &source),
                    ("a/x.luau", TARGET),
                    ("x.luau", TARGET),
                    ("shared/x.luau", TARGET),
                    ("a/near/x.luau", TARGET),
                    ("a/folder/init.luau", TARGET),
                    ("a/ambiguous.luau", TARGET),
                    ("a/ambiguous/init.luau", TARGET),
                    ("a/old.lua", "function f() end"),
                    ("a/run.server.luau", TARGET),
                    ("a/run.client.luau", TARGET),
                    ("Packages/x.luau", TARGET),
                ],
                &[
                    (
                        ".luaurc",
                        r#"{"aliases":{"shared":"./shared","near":"./wrong"}}"#,
                    ),
                    ("a/.luaurc", r#"{"aliases":{"near":"./near"}}"#),
                ],
            );
            let import_keys = blob
                .refs
                .iter()
                .zip(blob.reference_keys())
                .filter(|(r, _)| r.kind == BlobRefKind::Import)
                .map(|(_, k)| k)
                .collect::<BTreeSet<_>>();
            let import_rows = rows
                .iter()
                .filter(|r| import_keys.contains(&r.ref_ordinal))
                .collect::<Vec<_>>();
            assert_eq!(import_rows.len(), 1, "{caller}: {form}");
            assert_eq!(
                import_rows[0].target_path.as_deref(),
                expected.map(str::as_bytes),
                "{caller}: {form}"
            );
            let member_rows = rows
                .iter()
                .filter(|r| r.kind == BlobRefKind::Call && r.target_path.is_some())
                .collect::<Vec<_>>();
            assert_eq!(
                member_rows.len(),
                if expected.is_some() { 2 } else { 0 },
                "{caller}: {form}: {rows:?}"
            );
            for row in member_rows {
                assert_eq!(
                    row.target_path.as_deref(),
                    expected.map(str::as_bytes),
                    "{form}"
                );
                assert!(
                    matches!(
                        row.target_symbol.as_deref(),
                        Some("<return>::f" | "<return>::m")
                    ),
                    "{row:?}"
                );
            }
            for path in [".luaurc", "a/.luaurc"] {
                assert!(
                    binding.dependencies.contains(path),
                    "{form}: missing {path}"
                );
                assert!(
                    binding.dependencies.contains(&dependency::file(path)),
                    "{form}: missing resolver key {path}"
                );
            }
            if let Some(target) = expected {
                let stem = target
                    .strip_suffix("/init.luau")
                    .unwrap_or_else(|| target.strip_suffix(".luau").unwrap());
                for path in [format!("{stem}.luau"), format!("{stem}/init.luau")] {
                    assert!(
                        binding.dependencies.contains(&dependency::file(&path)),
                        "{form}: {path}"
                    );
                }
            }
        }
    }

    #[test]
    fn luau_s3_member_edges_follow_only_single_write_exports_and_direct_local_bindings() {
        for (target, member, expected) in [
            (
                "local T = { f = function() end }; return T",
                "f",
                Some("T::f"),
            ),
            (
                "local T = {}; function T.f() end; return T",
                "f",
                Some("T::f"),
            ),
            (
                "local T = {}; function T:m() end; return T",
                "m",
                Some("T::m"),
            ),
            (
                "local T = {}; T.f = function() end; return T",
                "f",
                Some("T::f"),
            ),
            ("local function f() end; return { g = f }", "g", Some("f")),
            ("return { h = function() end }", "h", Some("<return>::h")),
            (
                "local T = { f = function() end }; T.f = nil; return T",
                "f",
                None,
            ),
            (
                "local T = {}; function T.f() end; function T.f() end; return T",
                "f",
                None,
            ),
            (
                "local T = {}; function T.f() end; T.f = 1; return T",
                "f",
                None,
            ),
            (
                "local T = {}; function T.f() end; T = {}; return T",
                "f",
                None,
            ),
            ("local T = {}; function f() end; return T", "f", None),
            (
                "local T = {}; function T.f() end; if x then return T end; return T",
                "f",
                None,
            ),
            (
                "local T = {}; function T.f() end; return make(T)",
                "f",
                None,
            ),
            ("local T = { [name] = function() end }; return T", "f", None),
        ] {
            let caller =
                format!("local M = require('./target')\nlocal function caller() M.{member}() end");
            let (_, rows, _) = resolve(
                "caller.luau",
                &[("caller.luau", &caller), ("target.luau", target)],
                &[],
            );
            let calls = rows
                .iter()
                .filter(|r| r.kind == BlobRefKind::Call && r.target_symbol.is_some())
                .collect::<Vec<_>>();
            assert_eq!(
                calls.len(),
                usize::from(expected.is_some()),
                "{target}: {rows:?}"
            );
            if let Some(expected) = expected {
                assert_eq!(calls[0].target_symbol.as_deref(), Some(expected));
            }
        }
        for source in [
            "M = require('./target'); local function caller() M.f() end",
            "local M = require('./target'); M = other; local function caller() M.f() end",
            "local M = wrap(require('./target')); local function caller() M.f() end",
            "local M = require('./target'); local function caller(M) M.f() end",
            "local M = require('./target'); local M = other; local function caller() M.f() end",
            "local function owner() local M = require('./target') end; local function caller() M.f() end",
            "local function caller() M.f() end; local M = require('./target')",
        ] {
            let (_, rows, _) = resolve(
                "caller.luau",
                &[("caller.luau", source), ("target.luau", TARGET)],
                &[],
            );
            assert!(
                rows.iter()
                    .filter(|r| r.kind == BlobRefKind::Call)
                    .all(|r| r.target_symbol.is_none()),
                "{source}: {rows:?}"
            );
        }
        let (_, rows, _) = resolve(
            "caller.luau",
            &[
                (
                    "caller.luau",
                    "local function caller() local M = require('./target'); M.f() end",
                ),
                ("target.luau", TARGET),
            ],
            &[],
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == BlobRefKind::Call
                    && r.target_symbol.as_deref() == Some("<return>::f"))
                .count(),
            1
        );
    }
}
