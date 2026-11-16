pub use aft_parse::callgraph_store::luau::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::{join::dispatch::dependency, FileCallData, ResolverIndex};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeSet, HashMap};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;

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
