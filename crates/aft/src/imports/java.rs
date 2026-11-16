pub use aft_parse::imports::java::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_java(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Java).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = crate::imports::parse_imports(source, &tree, LangId::Java);
        (tree, block)
    }

    fn structured_modifiers(import: &ImportStatement) -> &[String] {
        match &import.form {
            ImportForm::Structured {
                named: _,
                namespace,
                alias,
                modifiers,
                import_kind,
            } => {
                assert!(namespace.is_none());
                assert!(alias.is_none());
                assert!(import_kind.is_none());
                modifiers
            }
            other => panic!("expected Structured form, got {other:?}"),
        }
    }

    /// Grammar fixture: lock the tree-sitter-java node kinds the parser depends
    /// on. If the grammar updates and renames these, this test fails loudly.
    #[test]
    fn java_grammar_node_kinds_are_stable() {
        let grammar = grammar_for(LangId::Java).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let src = "package com.example;\n\nimport java.util.List;\nimport static java.util.Collections.emptyList;\nimport java.util.*;\nimport static java.util.Arrays.*;\n\nclass C {}\n";
        let tree = parser.parse(src, None).unwrap();
        let mut kinds: BTreeSet<String> = BTreeSet::new();
        fn walk(node: tree_sitter::Node, kinds: &mut BTreeSet<String>) {
            kinds.insert(node.kind().to_string());
            let mut c = node.walk();
            if c.goto_first_child() {
                loop {
                    walk(c.node(), kinds);
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        walk(tree.root_node(), &mut kinds);
        for required in [
            "import_declaration",
            "scoped_identifier",
            "static",
            "asterisk",
            "import",
            ";",
        ] {
            assert!(
                kinds.contains(required),
                "java grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_java_all_four_forms() {
        let (_, block) = parse_java(
            "package com.example;\n\nimport java.util.List;\nimport static java.util.Collections.emptyList;\nimport java.util.*;\nimport static java.util.Arrays.*;\n\nclass C {}\n",
        );
        assert_eq!(block.imports.len(), 4);

        assert_eq!(block.imports[0].module_path, "java.util.List");
        assert_eq!(block.imports[0].names, Vec::<String>::new());
        assert_eq!(block.imports[0].default_import, None);
        assert_eq!(block.imports[0].namespace_import, None);
        assert_eq!(block.imports[0].kind, ImportKind::Value);
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
        assert!(structured_modifiers(&block.imports[0]).is_empty());

        assert_eq!(block.imports[1].module_path, "java.util.Collections");
        assert_eq!(block.imports[1].names, ["emptyList"]);
        match &block.imports[1].form {
            ImportForm::Structured { named, .. } => assert_eq!(named, &["emptyList"]),
            other => panic!("expected Structured form, got {other:?}"),
        }
        assert_eq!(
            structured_modifiers(&block.imports[1]),
            &["static".to_string()]
        );

        assert_eq!(block.imports[2].module_path, "java.util");
        assert_eq!(
            structured_modifiers(&block.imports[2]),
            &["wildcard".to_string()]
        );

        assert_eq!(block.imports[3].module_path, "java.util.Arrays");
        assert!(block.imports[3].names.is_empty());
        assert_eq!(
            structured_modifiers(&block.imports[3]),
            &["static".to_string(), "wildcard".to_string()]
        );
    }

    #[test]
    fn generate_java_all_forms() {
        assert_eq!(
            crate::imports::generate_import(
                LangId::Java,
                &ImportRequest::legacy("java.util.List", &[], None, None, false),
            ),
            "import java.util.List;"
        );

        let static_modifier = vec!["static".to_string()];
        assert_eq!(
            crate::imports::generate_import(
                LangId::Java,
                &ImportRequest {
                    module_path: "java.util.Collections.emptyList",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &static_modifier,
                    import_kind: None,
                },
            ),
            "import static java.util.Collections.emptyList;"
        );

        let wildcard_modifier = vec!["wildcard".to_string()];
        assert_eq!(
            crate::imports::generate_import(
                LangId::Java,
                &ImportRequest {
                    module_path: "java.util",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &wildcard_modifier,
                    import_kind: None,
                },
            ),
            "import java.util.*;"
        );

        let static_wildcard_modifiers = vec!["static".to_string(), "wildcard".to_string()];
        assert_eq!(
            crate::imports::generate_import(
                LangId::Java,
                &ImportRequest {
                    module_path: "java.util.Arrays",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &static_wildcard_modifiers,
                    import_kind: None,
                },
            ),
            "import static java.util.Arrays.*;"
        );
    }

    #[test]
    fn classify_java_groups() {
        assert_eq!(classify_group_java("java.util.List"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_java("javax.sql.DataSource"),
            ImportGroup::Stdlib
        );
        assert_eq!(
            classify_group_java("org.example.project.Foo"),
            ImportGroup::External
        );
        assert_eq!(
            classify_group_java("com.example.Foo"),
            ImportGroup::External
        );
    }

    #[test]
    fn java_round_trips_through_parse_generate() {
        for src in [
            "import java.util.List;",
            "import static java.util.Collections.emptyList;",
            "import java.util.*;",
            "import static java.util.Arrays.*;",
        ] {
            let (_, block) = parse_java(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let modifiers = structured_modifiers(imp);
            let regenerated = crate::imports::generate_import(
                LangId::Java,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers,
                    import_kind: None,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
