//! The call-graph extractor: one walk over each file's syntax tree.
//!
//! A [`SyntaxIndex`] walks the tree once; every pass of the extraction reads
//! the nodes, kinds and neighbours it recorded instead of walking the tree
//! again or asking tree-sitter to search for a parent. The passes are the
//! ones the extractor always ran, with the same rules; only how they find
//! their nodes changed. `CallgraphBlob::extract_reference` keeps the
//! extractor this replaced, for the extraction gate.
//!
//! This module also parses for extraction, including incremental reparsing
//! after an edit.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use super::syntax_index::SyntaxIndex;
use super::{
    assign_ordinals, blob_imports, blob_refs, blob_symbols, dispatch, language_id, structural_ref,
    AstPreorderNode, BlobRef, BlobRefKind, CallgraphBlob, ManifestJoinError, ParseBlob, Parser,
};
use crate::callgraph;
use crate::callgraph_store::facts::{FactPaths, NoProjectFacts};
use crate::parser::{grammar_for, LangId};

/// Parses `source` as `language` and extracts its parse output.
pub(super) fn extract(
    source: &str,
    language: &str,
    extractor_version: String,
) -> Result<CallgraphBlob, ManifestJoinError> {
    let lang = language_id(language)
        .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(language.to_string()))?;
    let tree = parse_source(source, lang)?;
    extract_tree(source, language, extractor_version, &tree, None)
}

/// Extracts the parse output of `source` from `tree`, its syntax tree in
/// `language`. With `outline`, also fills it with the file's symbols as the
/// parser extracts them, before the call graph changes any.
pub(super) fn extract_tree(
    source: &str,
    language: &str,
    extractor_version: String,
    tree: &tree_sitter::Tree,
    outline: Option<&mut Vec<crate::symbols::Symbol>>,
) -> Result<CallgraphBlob, ManifestJoinError> {
    let lang = language_id(language)
        .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(language.to_string()))?;
    let root = tree.root_node();
    let index = SyntaxIndex::new(root);
    let ast_nodes = ast_preorder_nodes(&index);
    let line_index = crate::callgraph_store::LineIndex::new(source);
    let mut data = callgraph::build_file_data_from_index(
        Path::new("__callgraph_blob__"),
        source,
        lang,
        tree,
        &line_index,
        &index,
        outline,
    )
    .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
    if lang == LangId::Rust {
        crate::callgraph_store::extend_rust_imports_from_uses(
            source,
            index
                .preorder_kinds()
                .filter(|(_, kind)| *kind == "use_declaration")
                .map(|(node, _)| node),
            &mut data,
        );
    }
    let mut symbols = blob_symbols(&data);
    let mut imports = blob_imports(&data);
    let mut refs = blob_refs(&line_index, &data);
    if lang == LangId::Rust {
        refs.extend(rust_module_refs(source, &index));
    }
    let paths = FactPaths {
        root: Path::new("/"),
        facts: &NoProjectFacts,
    };
    let file = Path::new("/__callgraph_blob__");
    let mut structural = crate::callgraph_store::collect_reexport_refs_with_line_index(
        paths.root,
        file,
        "__callgraph_blob__",
        source,
        lang,
        &crate::callgraph::ModuleResolutionMemo::default(),
        &paths,
        &line_index,
        Some(root),
    )
    .raw_refs;
    structural.extend(
        crate::callgraph_store::collect_source_less_export_alias_refs_with_line_index(
            "__callgraph_blob__",
            source,
            &line_index,
        )
        .raw_refs,
    );
    if lang == LangId::Rust {
        structural.extend(
            crate::callgraph_store::collect_rust_pub_use_reexport_refs(
                paths.root,
                file,
                "__callgraph_blob__",
                &data.import_block.imports,
                &line_index,
                &paths,
            )
            .raw_refs,
        );
    }
    refs.extend(structural.into_iter().map(structural_ref));
    assign_ordinals(
        &ast_nodes,
        &line_index,
        &mut symbols,
        &mut imports,
        &mut refs,
    );
    let mut exported_symbols = data.exported_symbols.clone();
    exported_symbols.sort();
    let mut callable_symbols = data.calls_by_symbol.keys().cloned().collect::<Vec<_>>();
    callable_symbols.sort();
    refs.sort_by(|left, right| {
        (
            left.ordinal,
            left.kind,
            left.byte_start,
            left.byte_end,
            left.full_ref.as_deref(),
            left.caller_symbol.as_deref(),
        )
            .cmp(&(
                right.ordinal,
                right.kind,
                right.byte_start,
                right.byte_end,
                right.full_ref.as_deref(),
                right.caller_symbol.as_deref(),
            ))
    });
    refs.dedup_by(|left, right| {
        left.ordinal == right.ordinal
            && left.kind == right.kind
            && left.byte_start == right.byte_start
            && left.byte_end == right.byte_end
            && left.full_ref == right.full_ref
            && left.caller_symbol == right.caller_symbol
    });

    let mut parse = ParseBlob {
        entry_point_symbols: data
            .symbol_metadata
            .iter()
            .filter(|(_, meta)| meta.entry_point_attribute.is_some())
            .map(|(name, _)| name.clone())
            .collect(),
        dispatch: dispatch::DispatchFacts::default(),
        extractor_version,
        language: language.to_string(),
        ast_nodes,
        symbols,
        default_export_symbol: data.default_export_symbol,
        exported_symbols,
        callable_symbols,
        imports,
        python_bindings: data.python_bindings,
        refs,
    };
    parse.entry_point_symbols.sort();
    parse.dispatch = dispatch::extract_with(source, lang, root, &mut parse, &index)?;
    Ok(CallgraphBlob::Parse(parse))
}

/// Every node's kind and span in preorder, numbered by position.
fn ast_preorder_nodes(index: &SyntaxIndex<'_>) -> Vec<AstPreorderNode> {
    index
        .preorder_kinds()
        .enumerate()
        .map(|(ordinal, (node, kind))| AstPreorderNode {
            ordinal: ordinal as u32,
            kind: Cow::Borrowed(kind),
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
        })
        .collect()
}

/// A reference for each `mod name;` declaration (a module whose body is in
/// another file).
fn rust_module_refs(source: &str, index: &SyntaxIndex<'_>) -> Vec<BlobRef> {
    let mut refs = Vec::new();
    for (node, kind) in index.preorder_kinds() {
        if kind != "mod_item"
            || node
                .named_children(&mut node.walk())
                .any(|child| child.kind() == "declaration_list")
        {
            continue;
        }
        let Some(name) = node.child_by_field_name("name") else {
            continue;
        };
        let module_name = source[name.byte_range()].to_string();
        refs.push(BlobRef {
            ordinal: 0,
            kind: BlobRefKind::Module,
            caller_symbol: None,
            short_name: Some(module_name.clone()),
            full_ref: Some(
                if crate::callgraph_store::rust_module_has_macro_use_with(source, node, index) {
                    format!("#[macro_use] mod {module_name};")
                } else {
                    module_name.clone()
                },
            ),
            module_path: Some(module_name),
            line: node.start_position().row as u32 + 1,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            path_override: crate::callgraph_store::rust_module_path_override_with(
                source, node, index,
            )
            .map(str::to_string),
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard: false,
            import_kind: None,
        });
    }
    refs
}

/// Parses `source` as `lang` for extraction, reusing a header's tree when
/// its language was chosen by parsing it.
pub(crate) fn parse_source(
    source: &str,
    lang: LangId,
) -> Result<tree_sitter::Tree, ManifestJoinError> {
    if let Some(tree) = crate::parser::cached_header_tree(source, lang) {
        return Ok(tree);
    }
    with_parser(lang, |parser| parser.parse(source, None))
}

/// Parses `source` as `lang` after an edit from `previous_source`, whose
/// tree in the same language is `previous`. The edit given to tree-sitter is
/// the span between the texts' common prefix and common suffix; tree-sitter
/// reuses every subtree the edit does not touch, and the tree it returns is
/// the tree a fresh parse of `source` gives.
pub(crate) fn reparse_source(
    previous_source: &str,
    previous: &tree_sitter::Tree,
    source: &str,
    lang: LangId,
) -> Result<tree_sitter::Tree, ManifestJoinError> {
    let Some(edit) = text_edit(previous_source.as_bytes(), source.as_bytes()) else {
        return Ok(previous.clone());
    };
    let mut tree = previous.clone();
    tree.edit(&edit);
    with_parser(lang, |parser| parser.parse(source, Some(&tree)))
}

/// The single edit that turns `old` into `new`: everything between their
/// common prefix and their common suffix. `None` when they are equal.
fn text_edit(old: &[u8], new: &[u8]) -> Option<tree_sitter::InputEdit> {
    if old == new {
        return None;
    }
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let longest_suffix = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(longest_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    // Tree-sitter counts rows by '\n' and columns in bytes.
    let point = |text: &[u8], byte: usize| {
        let before = &text[..byte];
        let row = before.iter().filter(|&&b| b == b'\n').count();
        let line_start = before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        tree_sitter::Point::new(row, byte - line_start)
    };
    Some(tree_sitter::InputEdit {
        start_byte: prefix,
        old_end_byte: old.len() - suffix,
        new_end_byte: new.len() - suffix,
        start_position: point(old, prefix),
        old_end_position: point(old, old.len() - suffix),
        new_end_position: point(new, new.len() - suffix),
    })
}

thread_local! {
    // A parser is mutable and not shareable, so each extraction thread keeps
    // one per grammar instead of building one per file.
    static EXTRACTION_PARSERS: std::cell::RefCell<HashMap<LangId, Parser>> =
        std::cell::RefCell::new(HashMap::new());
}

fn with_parser(
    lang: LangId,
    parse: impl FnOnce(&mut Parser) -> Option<tree_sitter::Tree>,
) -> Result<tree_sitter::Tree, ManifestJoinError> {
    EXTRACTION_PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        let parser = match parsers.entry(lang) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let mut parser = Parser::new();
                let grammar = grammar_for(lang).map_err(super::extraction_error)?;
                if lang == LangId::Luau {
                    crate::grammar::prepare_parser(&mut parser)
                        .map_err(ManifestJoinError::GrammarUnavailable)?;
                }
                parser
                    .set_language(&grammar)
                    .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
                entry.insert(parser)
            }
        };
        parse(parser)
            .ok_or_else(|| ManifestJoinError::Parse("tree-sitter returned no tree".to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both extractors on real files of several languages; the gate
    /// (`data_layer::extract_gate`) runs the same check on whole
    /// repositories.
    #[test]
    fn single_walk_output_equals_the_replaced_extractor() {
        let sources = [
            (include_str!("rust_bindings.rs"), "rust"),
            (include_str!("dispatch.rs"), "rust"),
            (include_str!("../calls.rs"), "rust"),
            (
                include_str!("../../../../packages/aft-bridge/src/bash-hints.ts"),
                "typescript",
            ),
            (
                include_str!("../../../../benchmarks/aft-search/author_real_query.py"),
                "python",
            ),
            (
                include_str!("../../tests/store_diet/corpus/receiver-go/receiver.go"),
                "go",
            ),
            (
                "export default () => run(x => x.go());\nconst o = { m() { this.n(); } };\n",
                "javascript",
            ),
            ("{\"a\": [1, {\"b\": 2}]}", "json"),
        ];
        for (source, language) in sources {
            let mut single = extract(source, language, "test".to_string()).unwrap();
            let mut reference = CallgraphBlob::extract_reference(source, language, "test").unwrap();
            for blob in [&mut single, &mut reference] {
                if let CallgraphBlob::Parse(parse) = blob {
                    parse.ast_nodes.clear();
                }
            }
            assert!(single == reference, "{language}: outputs differ");
        }
    }

    #[test]
    fn text_edit_spans_the_changed_bytes() {
        assert!(text_edit(b"abc", b"abc").is_none());
        let edit = text_edit(b"ab\ncd\nef", b"ab\nXYZ\nef").unwrap();
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (3, 5, 6)
        );
        assert_eq!(edit.start_position, tree_sitter::Point::new(1, 0));
        assert_eq!(edit.old_end_position, tree_sitter::Point::new(1, 2));
        assert_eq!(edit.new_end_position, tree_sitter::Point::new(1, 3));
        // Repeated text: the prefix and suffix may not overlap.
        let edit = text_edit(b"aaaa", b"aa").unwrap();
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (2, 4, 2)
        );
    }
}
