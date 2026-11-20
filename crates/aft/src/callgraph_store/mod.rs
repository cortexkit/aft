pub use aft_parse::callgraph_store::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph::{self, FileCallData, TraceToSymbolCandidate};
#[cfg(test)]
#[allow(unused_imports)]
use crate::error::AftError;
#[cfg(test)]
#[allow(unused_imports)]
use crate::imports::{ImportForm, ImportGroup, ImportKind, ImportStatement};
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::{parse_source_with_cached_parser, LangId};
#[cfg(test)]
#[allow(unused_imports)]
use crate::symbols::SymbolKind;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use disk_facts::DiskFacts;
#[cfg(test)]
#[allow(unused_imports)]
use facts::{byte_path, EntryKind, FactPaths, ProjectFacts};
#[cfg(test)]
#[allow(unused_imports)]
use join::syntax_index::{Direct, Nav};
#[cfg(test)]
#[allow(unused_imports)]
use std::cell::RefCell;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
#[cfg(test)]
#[allow(unused_imports)]
use std::fmt;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::rc::Rc;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, OnceLock};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use std::time::{Duration, Instant};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::Node;

#[cfg(test)]
pub mod dead_code_projection;

#[cfg(test)]
#[path = "../../tests/store_diet/harness.rs"]
mod store_diet;

#[cfg(test)]
mod local_symbol_memo_tests {
    use super::*;

    #[test]
    fn local_symbol_memo_preserves_scan_results_and_builds_once_per_caller() {
        let file = "fixture.rs";
        let mut source = String::from(
            "fn local() {} fn ambiguous() {}\n\
             mod nested { fn ambiguous() {} fn scoped_only() {} }\n\
             struct Item; impl Item { fn method(&self) {} }\n",
        );
        for i in 0..512 {
            source.push_str(&format!("fn function_{i}() {{ local(); }}\n"));
        }
        let mut data = callgraph::build_file_data_from_source(Path::new(file), &source).unwrap();
        data.calls_by_symbol
            .insert("caller_only".into(), Vec::new());
        data.exported_symbols.push("nested::export_only".into());
        let index = ProjectIndex::from_parts(
            Path::new("/fixture"),
            HashMap::new(),
            HashMap::from([(file.to_string(), &data)]),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(Path::new("/fixture"))),
        );
        assert_eq!(
            rust_unique_file_level_function(&index, file, "local", "local", &data),
            Some(("resolved_local".into(), file.into(), "local".into()))
        );
        assert!(
            rust_unique_file_level_function(&index, file, "ambiguous", "ambiguous", &data)
                .is_none()
        );
        assert_eq!(index.local_symbol(file, &data, "ambiguous"), None);
        assert_eq!(
            index.local_symbol(file, &data, "nested::ambiguous"),
            Some("nested::ambiguous".into())
        );
        let queries = data
            .symbol_metadata
            .keys()
            .chain(data.calls_by_symbol.keys())
            .chain(data.exported_symbols.iter())
            .flat_map(|name| [name.clone(), name.rsplit("::").next().unwrap().to_string()])
            .chain(["missing".into(), "caller_only".into(), "export_only".into()])
            .collect::<Vec<_>>();
        let started = Instant::now();
        let expected = queries
            .iter()
            .map(|query| {
                callgraph::resolve_symbol_query_in_data(&data, Path::new(file), query).ok()
            })
            .collect::<Vec<_>>();
        let scan_elapsed = started.elapsed();
        let started = Instant::now();
        for _ in 0..8 {
            for (query, expected) in queries.iter().zip(&expected) {
                assert_eq!(&index.local_symbol(file, &data, query), expected, "{query}");
            }
        }
        let memo_elapsed = started.elapsed();
        assert_eq!(index.local_symbols.borrow().builds, 1);
        let other =
            callgraph::build_file_data_from_source(Path::new("other.rs"), "fn other() {}").unwrap();
        assert_eq!(index.local_symbol("other.rs", &other, "local"), None);
        assert_eq!(
            index.local_symbol("other.rs", &other, "other"),
            Some("other".into())
        );
        assert_eq!(index.local_symbols.borrow().builds, 2);
        // A later cold-build window can reparse the same caller path.
        index.local_symbols.borrow_mut().file = None;
        assert_eq!(index.local_symbol("other.rs", &data, "other"), None);
        assert_eq!(
            index.local_symbol("other.rs", &data, "local"),
            Some("local".into())
        );
        assert_eq!(index.local_symbols.borrow().builds, 3);
        eprintln!(
            "local symbol queries={} scan_ns_per_query={} memo_ns_per_query={} builds=1",
            queries.len(),
            scan_elapsed.as_nanos() / queries.len() as u128,
            memo_elapsed.as_nanos() / (queries.len() * 8) as u128,
        );
    }
}

#[cfg(test)]
mod rust_use_parser_tests {
    use super::*;

    fn import(raw_text: &str) -> ImportStatement {
        ImportStatement {
            module_path: String::new(),
            names: Vec::new(),
            default_import: Some("pub".to_string()),
            namespace_import: None,
            kind: ImportKind::Value,
            group: ImportGroup::Internal,
            byte_range: 0..raw_text.len(),
            raw_text: raw_text.to_string(),
            form: ImportForm::RustUse {
                visibility: Some("pub".to_string()),
                named: Vec::new(),
            },
        }
    }

    #[test]
    fn rust_reexport_rejects_close_before_unclosed_group() {
        let raw_text = format!("pub use macro! {{}} {}::{{name;", "長".repeat(180));
        assert!(raw_text.len() > 541);
        assert_eq!(rust_pub_use_reexport_parts(&import(&raw_text)), None);
    }

    // Frozen pre-fix implementation: valid use declarations must retain their
    // existing extraction output, even when the text parser is imperfect.
    fn old_rust_pub_use_reexport_parts(
        import: &ImportStatement,
    ) -> Option<(String, HashMap<String, String>, bool)> {
        let body = rust_use_body(&import.raw_text).unwrap_or(import.module_path.as_str());
        let body = body.trim();
        if let Some(module_path) = body.strip_suffix("::*") {
            return Some((module_path.trim().to_string(), HashMap::new(), true));
        }

        if let Some(brace_start) = body.find("::{") {
            let module_path = body[..brace_start].trim().to_string();
            let names = rust_reexport_names_from_specs(&body[brace_start + 3..body.rfind('}')?]);
            if names.is_empty() {
                return None;
            }
            return Some((module_path, names, false));
        }

        let (module_path, spec) = body.rsplit_once("::")?;
        let names = rust_reexport_names_from_specs(spec);
        if names.is_empty() {
            return None;
        }
        Some((module_path.trim().to_string(), names, false))
    }

    #[test]
    fn rust_reexport_rejects_unclosed_groups() {
        for raw_text in [
            "pub use a::{",
            "pub use a::{b, c;",
            "pub use a::{b::{c, d};",
            "pub use a::{b /* } */;",
            "pub use a::{b // }\n;",
            "pub use a::{b /* nested /* } */ } */;",
        ] {
            assert_eq!(
                rust_pub_use_reexport_parts(&import(raw_text)),
                None,
                "{raw_text}"
            );
        }
    }

    #[test]
    fn rust_reexport_preserves_nested_groups_and_unicode() {
        let nested = rust_pub_use_reexport_parts(&import("pub use a::{b::{c, d}, e};"));
        assert_eq!(
            nested,
            Some((
                "a".into(),
                [
                    ("b::{c".into(), "b::{c".into()),
                    ("d}".into(), "d}".into()),
                    ("e".into(), "e".into())
                ]
                .into_iter()
                .collect(),
                false,
            ))
        );
        let unicode = rust_pub_use_reexport_parts(&import("pub use 長::道::{名 as 別名, café};"));
        assert_eq!(
            unicode,
            Some((
                "長::道".into(),
                [("別名".into(), "名".into()), ("café".into(), "café".into())]
                    .into_iter()
                    .collect(),
                false
            ))
        );
    }

    #[test]
    fn rust_use_group_matching_ignores_comments_and_literals() {
        for specs in [
            "b::{c, d}, e",
            "b /* } { /* } */ } */",
            "b // } {\n, c",
            r#"m!("} {", "escaped \" }", b"}", c"{")"#,
            r####"m!(r"}", r###"}"# {"###, br#"}"#, cr#"{"#)"####,
            r#"m!('}', '{', '\'', '\\', '\u{7d}', '長', 'a)"#,
        ] {
            let body = format!("a::{{{specs}}} trailing {{}}");
            assert_eq!(rust_use_group_close(&body), Some(4 + specs.len()), "{body}");
            let unclosed = format!("pub use a::{{{specs};");
            assert_eq!(
                rust_pub_use_reexport_parts(&import(&unclosed)),
                None,
                "{unclosed}"
            );
        }
        for raw_text in [
            "pub use a::{m!(\"}\");",
            "pub use a::{m!(\"unterminated }",
            "pub use a::{m!(r###\"unterminated }\"##);",
            "pub use a::{b /* unterminated }",
            "pub use a /* ::{b} */ ::c;",
            "pub use macro!(\"::{b}\");",
        ] {
            assert_eq!(
                rust_pub_use_reexport_parts(&import(raw_text)),
                None,
                "{raw_text}"
            );
        }
    }

    #[test]
    fn rust_reexport_preserves_legacy_comment_ranges() {
        // These imperfect names are kept for output compatibility.
        for (raw_text, module, name) in [
            ("pub use a::{b} /* } */;", "a", "b} /*"),
            ("pub use a /* ::{ */ ::{b};", "a /*", "*/ ::{b"),
        ] {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(raw_text, None).unwrap();
            assert!(!tree.root_node().has_error(), "{raw_text}");
            let node = tree.root_node().named_child(0).unwrap();
            let parsed = rust_import_from_use_node(raw_text, node).unwrap();
            assert_eq!(parsed.raw_text, raw_text);
            assert_eq!(
                rust_pub_use_reexport_parts(&parsed),
                Some((
                    module.into(),
                    [(name.into(), name.into())].into_iter().collect(),
                    false
                ))
            );
            assert_eq!(
                rust_pub_use_reexport_parts(&parsed),
                old_rust_pub_use_reexport_parts(&parsed)
            );
        }
    }

    struct Rng(u64);

    impl Rng {
        fn pick<'a>(&mut self, choices: &'a [&'a str]) -> &'a str {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            choices[(self.0 % choices.len() as u64) as usize]
        }
    }

    fn generated_use_spec(rng: &mut Rng, depth: usize) -> String {
        let name = rng.pick(&["b", "名", "café", "r#type", "long_name"]);
        match rng.pick(&["plain", "alias", "nested", "comment"]) {
            "alias" => format!("{name} as {}", rng.pick(&["別名", "renamed", "r#match"])),
            "nested" if depth > 0 => format!(
                "{name}::{{{}, {}}}",
                generated_use_spec(rng, depth - 1),
                generated_use_spec(rng, depth - 1)
            ),
            "comment" => format!(
                "{name} /* {} */",
                rng.pick(&["}", "{", "/* } */", "\"::{\"", "長"])
            ),
            _ => name.to_string(),
        }
    }

    #[test]
    fn rust_use_generated_inputs_never_panic_and_preserve_well_formed_output() {
        let mut rng = Rng(0x937a_b501_4cdf_628e);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        for _ in 0..4096 {
            let prefix = rng.pick(&["a", "長::道", "crate::r#type", "::外部"]);
            let spec = generated_use_spec(&mut rng, 3);
            let gap = rng.pick(&["", " /* ::{ */", " /* } */"]);
            let suffix = rng.pick(&["", " /* } */", " /* { */", " // }\n"]);
            let body = match rng.pick(&["group", "named", "wildcard"]) {
                "named" => format!("{prefix}::{spec}"),
                "wildcard" => format!("{prefix}::*"),
                _ => format!("{prefix}{gap}::{{{spec}, self,}}{suffix}"),
            };
            let raw_text = format!(
                "{}use {body};",
                rng.pick(&["pub ", "pub(crate) ", "pub(super) ", ""])
            );
            let parsed = import(&raw_text);
            assert!(
                !parser
                    .parse(&raw_text, None)
                    .unwrap()
                    .root_node()
                    .has_error(),
                "{raw_text}"
            );
            assert_eq!(
                rust_pub_use_reexport_parts(&parsed),
                old_rust_pub_use_reexport_parts(&parsed),
                "{raw_text}"
            );

            let mut malformed = format!("pub use {body}");
            for _ in 0..12 {
                malformed.push_str(rng.pick(&[
                    "::{", "}", "{", ";", "'", "\"", "\\", "/*", "*/", "//", "\n", "名", "🦀",
                    " as ", "r###\"", "#", ",",
                ]));
            }
            let result = std::panic::catch_unwind(|| {
                let parsed = import(&malformed);
                rust_use_body(&malformed);
                rust_use_visibility(&malformed);
                rust_use_list_names(&malformed);
                rust_reexport_names_from_specs(&malformed);
                quoted_module_path(&malformed);
                parse_reexport_names(&malformed);
                rust_pub_use_reexport_parts(&parsed);
            });
            assert!(result.is_ok(), "{malformed}");
        }
    }
}

#[cfg(test)]
mod crate_scan_memo_tests {
    //! The in-memory index answers `module_parent` and
    //! `inline_scoped_target` from a memo built once per index. These tests
    //! pin the memo to the scans it replaces, including their first-match
    //! order, and show that a changed file set (a new index) never reuses an
    //! earlier index's answers.
    use super::*;

    fn rust_file(declared: &[(&str, &str)], scoped: &[&str]) -> DbFileIndex {
        let mut index = DbFileIndex::empty(Some(LangId::Rust));
        for (module, target) in declared {
            index
                .declared_module_targets
                .insert((*module).to_string(), Some((*target).to_string()));
        }
        for symbol in scoped {
            index
                .node_by_scoped
                .insert((*symbol).to_string(), format!("node:{symbol}"));
        }
        index
    }

    fn index(files: Vec<(&str, DbFileIndex)>) -> ProjectIndex<'static> {
        ProjectIndex::from_parts(
            Path::new("/fixture"),
            files
                .into_iter()
                .map(|(path, index)| (path.to_string(), index))
                .collect(),
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(Path::new("/fixture"))),
        )
    }

    fn fixture_files() -> Vec<(&'static str, DbFileIndex)> {
        // A non-Rust file whose path sorts before the Rust match; the
        // language check must skip it.
        let mut untyped = DbFileIndex::empty(None);
        untyped
            .node_by_scoped
            .insert("deep::f".to_string(), "node:deep::f".to_string());
        vec![
            (
                "crates/a/src/lib.rs",
                rust_file(
                    &[
                        ("foo", "crates/a/src/foo.rs"),
                        ("alias_foo", "crates/a/src/foo.rs"),
                    ],
                    &["foo::inner::helper", "bar::deep::f"],
                ),
            ),
            (
                "crates/a/src/zed.rs",
                rust_file(&[("foo", "crates/a/src/foo.rs")], &[]),
            ),
            ("crates/a/src/foo.rs", rust_file(&[], &["inner::helper"])),
            ("crates/a/src/other.rs", rust_file(&[], &[])),
            ("crates/a/src/bar.rs", untyped),
            (
                "crates/b/src/lib.rs",
                rust_file(&[], &["foo::inner::helper"]),
            ),
            ("crates/b/src/x.rs", rust_file(&[], &[])),
        ]
    }

    fn segments(path: &[&str]) -> Vec<String> {
        path.iter().map(|segment| segment.to_string()).collect()
    }

    /// Compares every memo answer with the scan over the same index and
    /// returns the memo answers for further checks.
    fn assert_memo_matches_scan(
        index: &ProjectIndex<'_>,
    ) -> Vec<(String, Option<(String, String)>)> {
        let mut answers = Vec::new();
        let targets = [
            "crates/a/src/foo.rs",
            "crates/a/src/zed.rs",
            "crates/a/src/missing.rs",
        ];
        for target in targets {
            let memo = ResolverIndex::module_parent(index, target);
            assert_eq!(memo, index.module_parent_by_scan(target), "{target}");
            answers.push((format!("parent {target}"), memo));
        }
        let mut callers = index.files.keys().cloned().collect::<Vec<_>>();
        callers.push("crates/a/src/not_indexed.rs".to_string());
        callers.sort();
        let queries = [
            (segments(&["foo", "inner"]), "helper"),
            (segments(&["bar", "deep"]), "f"),
            (segments(&["foo"]), "absent"),
            (segments(&[]), "helper"),
        ];
        for caller in &callers {
            for (module_segments, name) in &queries {
                let memo =
                    ResolverIndex::inline_scoped_target(index, caller, module_segments, name);
                assert_eq!(
                    memo,
                    index.inline_scoped_target_by_scan(caller, module_segments, name),
                    "{caller} {module_segments:?} {name}"
                );
                answers.push((format!("inline {caller} {module_segments:?} {name}"), memo));
            }
        }
        answers
    }

    fn pair(file: &str, item: &str) -> Option<(String, String)> {
        Some((file.to_string(), item.to_string()))
    }

    #[test]
    fn rust_module_path_memo_preserves_cycles_missing_parents_and_path_fallbacks() {
        let index = index(vec![
            ("src/a.rs", rust_file(&[("b", "src/b.rs")], &[])),
            ("src/b.rs", rust_file(&[("a", "src/a.rs")], &[])),
            ("src/loose.rs", rust_file(&[], &[])),
            ("src/lib.rs", rust_file(&[], &[])),
        ]);
        for caller in [
            "src/a.rs",
            "src/b.rs",
            "src/loose.rs",
            "src/lib.rs",
            "src/missing.rs",
        ] {
            let expected = rust_caller_module_segments_uncached(&index, caller);
            assert_eq!(rust_caller_module_segments(&index, caller), expected);
            assert_eq!(rust_caller_module_segments(&index, caller), expected);
        }
    }

    #[test]
    fn memoized_crate_scans_answer_exactly_like_the_scans() {
        let index = index(fixture_files());
        assert_memo_matches_scan(&index);
        // Several declarations of one file: the smallest (file, module) pair.
        assert_eq!(
            ResolverIndex::module_parent(&index, "crates/a/src/foo.rs"),
            pair("crates/a/src/lib.rs", "alias_foo")
        );
        let inner = segments(&["foo", "inner"]);
        // The caller's own file wins over an earlier path.
        assert_eq!(
            ResolverIndex::inline_scoped_target(&index, "crates/a/src/lib.rs", &inner, "helper"),
            pair("crates/a/src/lib.rs", "foo::inner::helper")
        );
        // Otherwise the first path in order, within the caller's crate only.
        assert_eq!(
            ResolverIndex::inline_scoped_target(&index, "crates/a/src/other.rs", &inner, "helper"),
            pair("crates/a/src/foo.rs", "inner::helper")
        );
        assert_eq!(
            ResolverIndex::inline_scoped_target(&index, "crates/b/src/x.rs", &inner, "helper"),
            pair("crates/b/src/lib.rs", "foo::inner::helper")
        );
        // A non-Rust file that sorts first is skipped.
        assert_eq!(
            ResolverIndex::inline_scoped_target(
                &index,
                "crates/a/src/other.rs",
                &segments(&["bar", "deep"]),
                "f"
            ),
            pair("crates/a/src/lib.rs", "bar::deep::f")
        );
    }

    #[test]
    fn a_changed_file_set_never_reuses_an_earlier_index_memo() {
        let first = index(fixture_files());
        let inner = segments(&["foo", "inner"]);
        // Build the first index's memo before the file set changes.
        let before = assert_memo_matches_scan(&first);
        assert_eq!(
            ResolverIndex::module_parent(&first, "crates/a/src/foo.rs"),
            pair("crates/a/src/lib.rs", "alias_foo")
        );
        assert_eq!(
            ResolverIndex::inline_scoped_target(&first, "crates/a/src/other.rs", &inner, "helper"),
            pair("crates/a/src/foo.rs", "inner::helper")
        );

        // Remove two files and add two, the way a join rebuilds its index
        // from the previous index's files.
        let mut files = (*first.files).clone();
        files.remove("crates/a/src/lib.rs");
        files.remove("crates/a/src/foo.rs");
        files.insert(
            "crates/a/src/aaa.rs".to_string(),
            rust_file(&[("foo", "crates/a/src/foo.rs")], &[]),
        );
        files.insert(
            "crates/a/src/foo/mod.rs".to_string(),
            rust_file(&[], &["inner::helper"]),
        );
        let second = ProjectIndex::from_parts(
            Path::new("/fixture"),
            files,
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(Path::new("/fixture"))),
        );
        let after = assert_memo_matches_scan(&second);
        assert_ne!(before, after, "the fixture must change some answers");
        assert_eq!(
            ResolverIndex::module_parent(&second, "crates/a/src/foo.rs"),
            pair("crates/a/src/aaa.rs", "foo")
        );
        assert_eq!(
            ResolverIndex::inline_scoped_target(&second, "crates/a/src/other.rs", &inner, "helper"),
            pair("crates/a/src/foo/mod.rs", "inner::helper")
        );
        // The first index still answers for its own, unchanged file set.
        assert_eq!(assert_memo_matches_scan(&first), before);
    }
}

#[cfg(test)]
mod reexport_resolution_tests {
    use super::*;

    fn barrel_index(files: Vec<(String, DbFileIndex)>) -> ProjectIndex<'static> {
        ProjectIndex {
            facts: Rc::new(DiskFacts::new(Path::new("/fixture"))),
            project_root: PathBuf::from("/fixture"),
            files: Arc::new(files.into_iter().collect()),
            caller_data: HashMap::new(),
            lazy: None,
            workspace_crate_prefixes: WorkspaceCratePrefixCache::default(),
            rust_crate_roots: callgraph::RustCrateRootMemo::default(),
            crate_scans: std::cell::OnceCell::new(),
            lazy_module_parents: std::cell::OnceCell::new(),
            lazy_inline_candidates: std::cell::OnceCell::new(),
            rust_caller_paths: RefCell::default(),
            go_packages: RefCell::default(),
            local_symbols: RefCell::default(),
        }
    }

    fn barrel_file(reexport_targets: &[&str]) -> DbFileIndex {
        DbFileIndex {
            lang: None,
            exports: HashSet::new(),
            default_export: None,
            export_aliases: HashMap::new(),
            node_by_scoped: HashMap::new(),
            node_by_bare: HashMap::new(),
            node_kind_by_id: HashMap::new(),
            module_targets: HashMap::new(),
            declared_module_targets: HashMap::new(),
            macro_use_module_targets: BTreeSet::new(),
            reexports: reexport_targets
                .iter()
                .map(|target| ReexportIndex {
                    target_file: Some((*target).to_string()),
                    named: HashMap::new(),
                    wildcard: true,
                })
                .collect(),
        }
    }

    /// A dense wildcard re-export cycle (barrel files re-exporting each
    /// other) must resolve in O(files), not O(branching^depth). Without the
    /// resolver's memoization, resolving a MISSING symbol through this
    /// 12-file complete digraph explores ~11^16 paths and this test never
    /// finishes: the depth cap bounds path length, not path count, and one
    /// such resolution can pin a worker thread at 100% CPU indefinitely.
    #[test]
    fn missing_symbol_in_dense_wildcard_reexport_cycle_terminates() {
        let names: Vec<String> = (0..12).map(|i| format!("src/barrel{i}.ts")).collect();
        let files = names
            .iter()
            .map(|name| {
                let targets: Vec<&str> = names
                    .iter()
                    .filter(|other| *other != name)
                    .map(String::as_str)
                    .collect();
                (name.clone(), barrel_file(&targets))
            })
            .collect();
        let index = barrel_index(files);

        assert_eq!(
            resolve_exported_symbol(&index, "src/barrel0.ts", "does_not_exist", 0),
            None
        );
    }

    /// Depth-dominance counterexample: the walk first reaches `shared` down a
    /// 16-hop chain (no budget left for its leaf), then reaches it again
    /// directly at depth 1. Plain visited-set pruning would skip the second
    /// visit and lose a resolution the capped resolver finds; the
    /// depth-dominance memo revisits because the second arrival is shallower.
    #[test]
    fn shallow_revisit_after_deep_capped_visit_still_resolves() {
        let mut leaf = barrel_file(&[]);
        leaf.exports.insert("deep_symbol".to_string());
        let mut files: Vec<(String, DbFileIndex)> = Vec::new();
        // entry -> chain0 -> chain1 -> ... -> chain14 -> shared -> leaf
        // entry's SECOND reexport goes straight to shared.
        files.push((
            "src/entry.ts".to_string(),
            barrel_file(&["src/chain0.ts", "src/shared.ts"]),
        ));
        for i in 0..15 {
            let next = if i == 14 {
                "src/shared.ts".to_string()
            } else {
                format!("src/chain{}.ts", i + 1)
            };
            files.push((format!("src/chain{i}.ts"), barrel_file(&[&next])));
        }
        files.push(("src/shared.ts".to_string(), barrel_file(&["src/leaf.ts"])));
        files.push(("src/leaf.ts".to_string(), leaf));
        let index = barrel_index(files);

        assert_eq!(
            resolve_exported_symbol(&index, "src/entry.ts", "deep_symbol", 0),
            Some(("src/leaf.ts".to_string(), "deep_symbol".to_string())),
            "a shallower re-visit must not be pruned by a deeper capped visit"
        );
    }

    #[test]
    fn symbol_reachable_through_reexport_cycle_still_resolves() {
        let mut leaf = barrel_file(&[]);
        leaf.exports.insert("real_symbol".to_string());
        let index = barrel_index(vec![
            (
                "src/a.ts".to_string(),
                barrel_file(&["src/b.ts", "src/a.ts"]),
            ),
            (
                "src/b.ts".to_string(),
                barrel_file(&["src/a.ts", "src/leaf.ts"]),
            ),
            ("src/leaf.ts".to_string(), leaf),
        ]);

        assert_eq!(
            resolve_exported_symbol(&index, "src/a.ts", "real_symbol", 0),
            Some(("src/leaf.ts".to_string(), "real_symbol".to_string()))
        );
    }
}

#[cfg(test)]
mod canonicalize_path_tests {
    use super::*;

    fn canonicalize_calls<T>(run: impl FnOnce() -> T) -> (T, usize) {
        let before = CANONICALIZE_CALLS.with(std::cell::Cell::get);
        let value = run();
        (
            value,
            CANONICALIZE_CALLS.with(std::cell::Cell::get) - before,
        )
    }

    fn deep_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let deep = root.join("packages/plugin/src/features/magic-context/dreamer/nested");
        std::fs::create_dir_all(&deep).expect("deep dirs");
        (dir, root, deep)
    }

    // The resolver probes a missing `tsconfig.json` in every ancestor of every
    // importing file. Each probe must cost a fixed number of `realpath` calls,
    // not one per path component: on a real checkout the per-component walk
    // made a callgraph refresh issue millions of `getattrlist` calls a minute.
    #[test]
    fn missing_probe_canonicalizes_its_parent_not_every_ancestor() {
        let (_dir, root, deep) = deep_fixture();
        let probe = deep.join("tsconfig.json");

        let (canonical, calls) = canonicalize_calls(|| canonicalize_path(&probe));
        assert_eq!(canonical, probe);
        assert!(
            calls <= 2,
            "one missing probe took {calls} canonicalize calls for {} components",
            probe.components().count()
        );

        let disk = disk_facts::DiskFacts::new(&root);
        let facts = facts::FactPaths {
            root: &root,
            facts: &disk,
        };
        let (found, calls) = canonicalize_calls(|| facts.is_file(&probe));
        assert!(!found);
        assert!(calls <= 2, "resolver probe took {calls} canonicalize calls");
    }

    #[test]
    fn missing_path_shortcut_matches_the_component_walk() {
        let (_dir, root, deep) = deep_fixture();
        std::fs::write(deep.join("file.ts"), "").expect("file");
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cases = vec![
            deep.join("tsconfig.json"),
            deep.join("missing/also-missing/index.ts"),
            root.join("missing"),
            // A regular file used as a directory is not a missing ancestor.
            deep.join("file.ts/index.ts"),
        ];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&deep, root.join("linked")).expect("dir link");
            std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling"))
                .expect("dangling link");
            cases.push(root.join("linked/tsconfig.json"));
            cases.push(root.join("linked/missing/index.ts"));
            cases.push(root.join("dangling/index.ts"));
        }
        for case in cases {
            assert_eq!(
                canonicalize_path(&case),
                canonicalize_missing_path(&case),
                "{}",
                case.display()
            );
        }
        #[cfg(unix)]
        assert_eq!(
            canonicalize_path(&root.join("linked/tsconfig.json")),
            deep.join("tsconfig.json")
        );
    }
}

#[cfg(test)]
pub mod fact_answers;

#[cfg(test)]
pub mod go_dispatch;

#[cfg(test)]
pub mod join;

#[cfg(test)]
pub mod luau;

#[cfg(test)]
pub mod resolution_facts;

#[cfg(test)]
pub mod rust_bindings;
