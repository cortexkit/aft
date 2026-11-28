pub use aft_parse::callgraph_store::join::single_walk::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::syntax_index::SyntaxIndex;
#[cfg(test)]
#[allow(unused_imports)]
use super::{
    assign_ordinals, blob_imports, blob_refs, blob_symbols, dispatch, language_id, structural_ref,
    AstPreorderNode, BlobRef, BlobRefKind, CallgraphBlob, ManifestJoinError, ParseBlob, Parser,
};
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph;
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph_store::facts::{FactPaths, NoProjectFacts};
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::LangId;
#[cfg(test)]
#[allow(unused_imports)]
use std::borrow::Cow;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;

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
                include_str!("../../../aft/tests/store_diet/corpus/receiver-go/receiver.go"),
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
