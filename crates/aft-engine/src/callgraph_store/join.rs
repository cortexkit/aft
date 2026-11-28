pub use aft_parse::callgraph_store::join::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::facts::{BlobKey, FactPaths, NoProjectFacts, ProjectFacts};
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph::{self, FileCallData, SymbolMeta};
#[cfg(test)]
#[allow(unused_imports)]
use crate::imports::{ImportBlock, ImportForm, ImportGroup, ImportKind, ImportStatement};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use crate::parser::grammar_for;
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::LangId;
#[cfg(test)]
#[allow(unused_imports)]
use crate::symbols::SymbolKind;
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::borrow::Cow;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::fmt;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::rc::Rc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(not(test))]
use tree_sitter::Parser;

#[cfg(test)]
#[path = "extraction_work.rs"]
pub mod extraction_work;

#[cfg(test)]
#[path = "extraction_tests.rs"]
mod extraction_tests;

#[cfg(test)]
#[path = "payload_tests.rs"]
mod payload_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The replay's quick dependency check agrees with the full list for
    /// every kind of surface query, whichever of its dependencies changed.
    #[test]
    fn a_surface_query_depends_on_exactly_its_listed_dependencies() {
        let file = "crates/demo/src/a/b.rs".to_string();
        let symbol = "f".to_string();
        let queries = [
            ViewSurfaceQuery::Language(file.clone()),
            ViewSurfaceQuery::Module(file.clone(), symbol.clone()),
            ViewSurfaceQuery::MacroUse(file.clone()),
            ViewSurfaceQuery::Parent(file.clone()),
            ViewSurfaceQuery::Reexports(file.clone()),
            ViewSurfaceQuery::Node(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Callable(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Alias(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Export(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Default(file.clone()),
            ViewSurfaceQuery::Contains(file.clone()),
            ViewSurfaceQuery::Crate("demo".to_string()),
            ViewSurfaceQuery::CrateRoot(file.clone()),
            ViewSurfaceQuery::Inline(
                file.clone(),
                vec!["a".to_string(), "b".to_string()],
                symbol.clone(),
            ),
            ViewSurfaceQuery::GoPackage(file.clone(), Some("example.com/m".to_string())),
        ];
        let mut candidates = queries
            .iter()
            .flat_map(ViewSurfaceQuery::dependencies)
            .collect::<BTreeSet<_>>();
        candidates.insert("crates/demo/src/other.rs".to_string());
        candidates.insert(VIEW_GO_PACKAGE_DOMAIN.to_string());
        candidates.insert(VIEW_GO_MODULES_DOMAIN.to_string());
        for query in &queries {
            let listed = query.dependencies();
            assert!(!query.depends_on_any(&BTreeSet::new()), "{query:?}");
            for candidate in &candidates {
                let changed = BTreeSet::from([candidate.clone()]);
                assert_eq!(
                    query.depends_on_any(&changed),
                    listed.contains(candidate),
                    "{query:?} with {candidate:?} changed"
                );
            }
        }
    }

    #[test]
    fn blob_extraction_is_deterministic_for_nested_callers() {
        let source = include_str!("../../../../benchmarks/aft-search/author_real_query.py");
        let first = CallgraphBlob::extract(source, "python", "determinism-test-v1").unwrap();
        let expected = first.to_bytes().unwrap();
        for attempt in 1..32 {
            let actual = CallgraphBlob::extract(source, "python", "determinism-test-v1").unwrap();
            assert!(
                actual.to_bytes().unwrap() == expected,
                "serialization changed at attempt {attempt}"
            );
        }
        let call_start = source.find("subprocess.run(command,").unwrap();
        let callers = first
            .parse()
            .unwrap()
            .refs
            .iter()
            .filter(|r| {
                r.full_ref.as_deref() == Some("subprocess.run") && r.byte_start == call_start
            })
            .filter_map(|r| r.caller_symbol.as_deref())
            .collect::<BTreeSet<_>>();
        assert_eq!(callers, BTreeSet::from(["at_pin"]));

        // Check the construction order as well as final serialization: later
        // stable sorts preserve input order when several references share a key.
        // Fresh maps exercise different hash seeds without depending on a seed
        // exposed by the standard library.
        let expected = (0..64)
            .map(|i| format!("caller_{i:02}"))
            .collect::<Vec<_>>();
        for _ in 0..16 {
            let sites = expected
                .iter()
                .rev()
                .map(|name| (name.clone(), Vec::new()))
                .collect::<HashMap<_, _>>();
            let actual = ordered_symbol_sites(&sites)
                .into_iter()
                .map(|(caller, _)| caller)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn blob_ordinals_are_tree_sitter_preorder_positions() {
        let source = "export function run() { return helper(); }\nfunction helper() {}\n";
        let blob = CallgraphBlob::extract(source, "typescript", "join-test-v1").unwrap();
        let repeated = CallgraphBlob::extract(source, "typescript", "join-test-v1").unwrap();
        let different_version =
            CallgraphBlob::extract(source, "typescript", "join-test-v2").unwrap();
        assert_eq!(blob.to_bytes().unwrap(), repeated.to_bytes().unwrap());
        assert_ne!(
            blob.to_bytes().unwrap(),
            different_version.to_bytes().unwrap()
        );
        let parse = blob.parse().unwrap();
        assert_eq!(parse.ast_nodes[0].ordinal, 0);
        assert!(parse.refs.iter().all(|reference| parse
            .ast_nodes
            .iter()
            .any(|node| node.ordinal == reference.ordinal)));
        let helper = parse
            .refs
            .iter()
            .find(|reference| reference.short_name.as_deref() == Some("helper"))
            .unwrap();
        let node = parse
            .ast_nodes
            .iter()
            .find(|node| node.ordinal == helper.ordinal)
            .unwrap();
        assert!(node.byte_start <= helper.byte_start && node.byte_end >= helper.byte_end);
        assert_eq!(node.kind, "call_expression");
        let mut parser = Parser::new();
        parser
            .set_language(&grammar_for(LangId::TypeScript).unwrap())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut cursor = tree.walk();
        let mut expected = Vec::new();
        'preorder: loop {
            let node = cursor.node();
            expected.push(AstPreorderNode {
                ordinal: expected.len() as u32,
                kind: Cow::Borrowed(node.kind()),
                byte_start: node.start_byte(),
                byte_end: node.end_byte(),
            });
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'preorder;
                }
            }
        }
        assert_eq!(parse.ast_nodes, expected);
    }
}

#[cfg(test)]
#[path = "../../../aft/tests/integration/join_manifest_test.rs"]
mod disk_facts_tests;

#[cfg(test)]
mod consultation_tests {
    use super::*;

    #[test]
    fn opaque_config_read_is_unattributed_even_after_a_known_field_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("package.json"), b"{}").unwrap();
        let facts = ViewBindingFacts::new(Rc::new(super::super::disk_facts::DiskFacts::new(
            root.path(),
        )));
        facts.config_fact(b"package.json", "name");
        assert!(facts.attributed_config_bytes(b"package.json").is_some());
        assert!(!facts.consultations.borrow().unattributed());
        facts.memo_start(Path::new("/"), "test", "");
        assert!(facts.config_bytes(b"package.json").is_some());
        facts.memo_finish(Path::new("/"), "test", "");
        assert!(facts.take_config().unattributed());
        facts.memo_replay(Path::new("/"), "test", "");
        assert!(facts.take_config().unattributed());
    }
}

#[path = "dispatch.rs"]
#[cfg(test)]
pub mod dispatch;

#[path = "single_walk.rs"]
#[cfg(test)]
pub mod single_walk;

#[path = "syntax_index.rs"]
#[cfg(test)]
pub mod syntax_index;
