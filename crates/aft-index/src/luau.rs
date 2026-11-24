#[allow(unused_imports)]
pub use aft_parse::luau::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use crate::imports::{ImportBlock, ImportForm, ImportGroup, ImportKind, ImportStatement};
#[cfg(test)]
#[allow(unused_imports)]
use crate::symbols::SymbolKind;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::Node;

#[cfg(all(
    test,
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
pub(crate) mod tests {
    use super::*;
    use crate::parser::LangId;

    #[test]
    fn luau_s3_symbols_and_content_record_use_the_builtin_extraction_path() {
        ready();
        let (record, _) =
            crate::data_layer::content::extract_with_tree(FIXTURE.as_bytes(), "luau").unwrap();
        let aft_store::content::RecordBody::Parsed { body, .. } = record.data else {
            panic!("parsed record expected")
        };
        let tree = tree(FIXTURE);
        let symbols =
            crate::parser::extract_symbols_from_tree(FIXTURE, &tree, LangId::Luau).unwrap();
        let rows = symbols
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.kind.clone(),
                    s.parent.as_deref(),
                    s.exported,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                ("marker", SymbolKind::Variable, None, true),
                ("global", SymbolKind::Function, None, true),
                ("helper", SymbolKind::Function, None, true),
                ("T", SymbolKind::Variable, None, true),
                ("f", SymbolKind::Method, Some("T"), true),
                ("m", SymbolKind::Method, Some("T"), true),
                ("assigned", SymbolKind::Method, Some("T"), true),
                ("Private", SymbolKind::TypeAlias, None, false),
                ("Public", SymbolKind::TypeAlias, None, true),
                ("complex", SymbolKind::Function, None, true),
            ]
        );
        assert_eq!(body.outline.as_ref().unwrap().len(), rows.len());
        for (symbol, (start, end)) in symbols.iter().zip([
            (4, 4),
            (5, 5),
            (6, 6),
            (7, 7),
            (8, 8),
            (9, 9),
            (10, 10),
            (11, 11),
            (12, 12),
            (13, 25),
        ]) {
            assert_eq!(
                (
                    symbol.range.start_line,
                    symbol.range.start_col,
                    symbol.range.end_line
                ),
                (start, 0, end),
                "{}",
                symbol.name
            );
            assert_eq!(
                symbol.range.end_col as usize,
                FIXTURE.lines().nth(end as usize).unwrap().len(),
                "{}",
                symbol.name
            );
        }
        let assigned = symbols.iter().find(|s| s.name == "assigned").unwrap();
        assert_eq!(
            (
                assigned.range.start_line,
                assigned.range.start_col,
                assigned.range.end_line,
                assigned.range.end_col
            ),
            (10, 0, 10, 36)
        );
        let exported = symbols.iter().find(|s| s.name == "Public").unwrap();
        assert_eq!(
            (
                exported.range.start_line,
                exported.range.start_col,
                exported.range.end_col
            ),
            (12, 0, 34)
        );
        let comments =
            crate::parser::todo_markers::markers_from_tree(FIXTURE.as_bytes(), LangId::Luau, &tree);
        assert_eq!(
            comments
                .items
                .unwrap()
                .iter()
                .map(|item| item.line)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn luau_s3_exports_require_a_single_write_and_final_chunk_return() {
        for (source, expected) in [
            ("local T = { f = function() end }; return T", vec![("f", "T::f")]),
            ("local T = {}; function T.f() end; function T:m() end; T.h = function() end; return T", vec![("f", "T::f"), ("h", "T::h"), ("m", "T::m")]),
            ("local function f() end; return { g = f, h = function() end }", vec![("g", "f"), ("h", "<return>::h")]),
            ("local T = { f = function() end }; T.f = nil; return T", vec![]),
            ("local T = {}; function T.f() end; function T.f() end; return T", vec![]),
            ("local T = {}; function T.f() end; T.f = 1; return T", vec![]),
            ("local T = { f = function() end }; T = {}; return T", vec![]),
            ("local T = { f = function() end }; if x then return T end; return T", vec![]),
            ("local T = { f = function() end }; return T; local x = 1", vec![]),
            ("local function f() end; return make()", vec![]),
            ("local T = { [key] = function() end }; return T", vec![]),
            ("local T = {}; function f() end; return T", vec![]),
            ("local function f() end; f = nil; return { g = f }", vec![]),
        ] {
            ready();
            let tree = crate::parser::parse_luau_source(std::path::Path::new("exports.luau"), source).unwrap();
            let actual = exports(source, tree.root_node()).into_iter().map(|(name, export)| (name, export.symbol)).collect::<Vec<_>>();
            assert_eq!(actual, expected.into_iter().map(|(a,b)| (a.to_string(),b.to_string())).collect::<Vec<_>>(), "{source}");
        }
    }
}
