pub use aft_parse::imports::ruby::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::{Node, Tree};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_ruby(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Ruby).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Ruby);
        (tree, block)
    }

    fn structured(import: &ImportStatement) -> (&[String], Option<&str>) {
        match &import.form {
            ImportForm::Structured {
                named,
                namespace,
                alias,
                modifiers,
                import_kind,
            } => {
                assert!(named.is_empty());
                assert!(namespace.is_none());
                assert!(alias.is_none());
                (modifiers, import_kind.as_deref())
            }
            other => panic!("expected Ruby Structured import, got {other:?}"),
        }
    }

    /// Grammar fixture: lock the exact tree-sitter-ruby node kinds this parser
    /// depends on. The current grammar emits both bare `require 'x'` and
    /// parenthesized `require('x')` as top-level `call` nodes with an `identifier`
    /// method child and an `argument_list` containing a `string`.
    #[test]
    fn ruby_grammar_node_kinds_are_stable() {
        let src = "require 'foo'\nrequire(\"bar\")\nrequire_relative '../baz'\nload 'qux.rb'\nobj.require 'ignored'\nif true\n  require 'nested'\nend\n";
        let (tree, _) = parse_ruby(src);
        assert!(!tree.root_node().has_error());

        let mut kinds = BTreeSet::new();
        fn walk(node: Node, kinds: &mut BTreeSet<String>) {
            kinds.insert(node.kind().to_string());
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    walk(cursor.node(), kinds);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        walk(tree.root_node(), &mut kinds);

        for required in [
            "program",
            "call",
            "identifier",
            "argument_list",
            "string",
            "string_content",
            ".",
            "(",
            ")",
        ] {
            assert!(
                kinds.contains(required),
                "ruby grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_ruby_supported_top_level_forms() {
        let (_, block) = parse_ruby(
            "require 'foo'\nrequire \"foo/bar\"\nrequire('paren')\nrequire_relative '../baz'\nload 'qux.rb'\nautoload :Sym, 'path'\nobj.require 'not_top_level'\nif true\n  require 'nested'\nend\n",
        );
        assert_eq!(block.imports.len(), 5);

        assert_ruby_import(
            &block.imports[0],
            "foo",
            RUBY_REQUIRE_KIND,
            RUBY_QUOTE_SINGLE,
        );
        assert_ruby_import(
            &block.imports[1],
            "foo/bar",
            RUBY_REQUIRE_KIND,
            RUBY_QUOTE_DOUBLE,
        );
        assert_ruby_import(
            &block.imports[2],
            "paren",
            RUBY_REQUIRE_KIND,
            RUBY_QUOTE_SINGLE,
        );
        assert_ruby_import(
            &block.imports[3],
            "../baz",
            RUBY_REQUIRE_RELATIVE_KIND,
            RUBY_QUOTE_SINGLE,
        );
        assert_ruby_import(
            &block.imports[4],
            "qux.rb",
            RUBY_LOAD_KIND,
            RUBY_QUOTE_SINGLE,
        );
    }

    fn assert_ruby_import(
        imp: &ImportStatement,
        module_path: &str,
        import_kind: &str,
        quote_modifier: &str,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert!(imp.default_import.is_some());
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::SideEffect);
        assert_eq!(imp.group, ImportGroup::External);

        let (modifiers, parsed_import_kind) = structured(imp);
        assert_eq!(modifiers, &[quote_modifier.to_string()]);
        assert_eq!(parsed_import_kind, Some(import_kind));
    }

    #[test]
    fn generate_ruby_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Ruby,
                &ImportRequest::legacy("foo", &[], None, None, false)
            ),
            "require 'foo'"
        );
        assert_eq!(
            generate_import(
                LangId::Ruby,
                &ImportRequest {
                    module_path: "../baz",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some(RUBY_REQUIRE_RELATIVE_KIND),
                }
            ),
            "require_relative '../baz'"
        );
        assert_eq!(
            generate_import(
                LangId::Ruby,
                &ImportRequest {
                    module_path: "qux.rb",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some(RUBY_LOAD_KIND),
                }
            ),
            "load 'qux.rb'"
        );
    }

    #[test]
    fn generate_ruby_preserves_organized_flat_markers() {
        assert_eq!(
            generate_import(
                LangId::Ruby,
                &ImportRequest::legacy(
                    "../baz",
                    &[],
                    Some(RUBY_REQUIRE_RELATIVE_KIND),
                    None,
                    false
                )
            ),
            "require_relative '../baz'"
        );
        assert_eq!(
            generate_import(
                LangId::Ruby,
                &ImportRequest::legacy("qux.rb", &[], Some("load|double"), None, false)
            ),
            "load \"qux.rb\""
        );
    }

    #[test]
    fn classify_group_ruby_is_neutral_external() {
        assert_eq!(classify_group_ruby("json"), ImportGroup::External);
        assert_eq!(classify_group_ruby("./local"), ImportGroup::External);
        assert_eq!(classify_group_ruby("../relative"), ImportGroup::External);
    }

    #[test]
    fn ruby_round_trips_through_parse_generate() {
        for src in [
            "require 'foo'",
            "require \"foo/bar\"",
            "require_relative '../baz'",
            "load 'qux.rb'",
            "load \"other.rb\"",
        ] {
            let (_, block) = parse_ruby(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (modifiers, import_kind) = structured(imp);
            let regenerated = generate_import(
                LangId::Ruby,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: imp.default_import.as_deref(),
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers,
                    import_kind,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
