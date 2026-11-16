pub use aft_parse::imports::lua::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_lua(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Lua).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Lua);
        (tree, block)
    }

    /// Grammar fixture: lock the tree-sitter-lua node kinds this parser uses.
    /// The current grammar emits top-level `function_call` nodes for bare
    /// requires and `variable_declaration -> assignment_statement` for local
    /// bindings. The require argument lives under an `arguments` node as a
    /// direct `string` child in both parenthesized and bare-string forms.
    #[test]
    fn lua_grammar_node_kinds_are_stable() {
        let src = "local x = require(\"y\")\nrequire \"z\"\nrequire(\"w\")\nlocal bar = require(\"pkg.bar\")\n";
        let (tree, _) = parse_lua(src);
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
            "chunk",
            "variable_declaration",
            "local",
            "assignment_statement",
            "variable_list",
            "expression_list",
            "function_call",
            "identifier",
            "arguments",
            "string",
            "string_content",
        ] {
            assert!(
                kinds.contains(required),
                "lua grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_lua_all_supported_forms() {
        let (_, block) = parse_lua(
            "local foo = require(\"foo\")\nlocal bar = require \"pkg.bar\"\nrequire(\"side.effect\")\nrequire \"boot\"\n",
        );
        assert_eq!(block.imports.len(), 4);

        assert_lua_import(
            &block.imports[0],
            "foo",
            Some("foo"),
            ImportKind::Value,
            &[],
        );
        assert_lua_import(
            &block.imports[1],
            "pkg.bar",
            Some("bar"),
            ImportKind::Value,
            &[BARE_STRING_ARGUMENT_MODIFIER],
        );
        assert_lua_import(
            &block.imports[2],
            "side.effect",
            None,
            ImportKind::SideEffect,
            &[],
        );
        assert_lua_import(
            &block.imports[3],
            "boot",
            None,
            ImportKind::SideEffect,
            &[BARE_STRING_ARGUMENT_MODIFIER],
        );
    }

    fn assert_lua_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_binding: Option<&str>,
        expected_kind: ImportKind,
        expected_modifiers: &[&str],
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.default_import.as_deref(), expected_binding);
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, expected_kind);
        assert_eq!(imp.group, ImportGroup::External);

        assert_eq!(
            imp.form,
            ImportForm::Structured {
                named: vec![],
                namespace: None,
                alias: expected_binding.map(str::to_string),
                modifiers: expected_modifiers
                    .iter()
                    .map(|modifier| modifier.to_string())
                    .collect(),
                import_kind: None,
            }
        );
    }

    #[test]
    fn generate_lua_all_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest::legacy("foo", &[], Some("foo"), None, false)
            ),
            "local foo = require(\"foo\")"
        );
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest::legacy("side.effect", &[], None, None, false)
            ),
            "require(\"side.effect\")"
        );

        let bare_string_modifier = vec![BARE_STRING_ARGUMENT_MODIFIER.to_string()];
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest {
                    module_path: "pkg.bar",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("bar"),
                    type_only: false,
                    modifiers: &bare_string_modifier,
                    import_kind: None,
                }
            ),
            "local bar = require \"pkg.bar\""
        );
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest {
                    module_path: "boot",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &bare_string_modifier,
                    import_kind: None,
                }
            ),
            "require \"boot\""
        );
    }

    #[test]
    fn parse_lua_requires_one_local_binding_and_one_rhs_expression() {
        let (_, block) = parse_lua(
            "local pure = require(\"pure\")\n\
             local mixed = require(\"mixed\"), keep()\n\
             local two_requires = require(\"first\"), require(\"second\")\n\
             local first, second = require(\"first\"), require(\"second\")\n\
             plain = require(\"plain\")\n",
        );

        let modules: Vec<&str> = block
            .imports
            .iter()
            .map(|import| import.module_path.as_str())
            .collect();
        assert_eq!(modules, vec!["pure"]);
    }

    #[test]
    fn classify_group_lua_always_external() {
        assert_eq!(classify_group_lua("foo"), ImportGroup::External);
        assert_eq!(classify_group_lua("pkg.bar"), ImportGroup::External);
        assert_eq!(classify_group_lua("./local"), ImportGroup::External);
    }

    #[test]
    fn lua_round_trips_through_parse_generate() {
        for src in [
            "local foo = require(\"foo\")",
            "local bar = require \"pkg.bar\"",
            "require(\"side.effect\")",
            "require \"boot\"",
        ] {
            let (_, block) = parse_lua(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (alias, modifiers) = match &imp.form {
                ImportForm::Structured {
                    alias, modifiers, ..
                } => (alias.as_deref(), modifiers.as_slice()),
                other => panic!("expected Lua Structured import, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Lua,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: imp.default_import.as_deref(),
                    namespace: None,
                    alias,
                    type_only: false,
                    modifiers,
                    import_kind: None,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
