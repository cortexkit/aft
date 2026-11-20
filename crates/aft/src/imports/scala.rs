pub use aft_parse::imports::scala::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::{
    import_byte_range, sort_named_specifiers, ImportBlock, ImportForm, ImportGroup, ImportKind,
    ImportRequest, ImportStatement, ImportSyntax,
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

    fn parse_scala(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Scala).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Scala);
        (tree, block)
    }

    fn structured(import: &ImportStatement) -> (&[String], &[String], Option<&str>) {
        match &import.form {
            ImportForm::Structured {
                named,
                namespace,
                alias,
                modifiers,
                import_kind,
            } => {
                assert_eq!(namespace, &None);
                assert_eq!(alias, &None);
                (named, modifiers, import_kind.as_deref())
            }
            other => panic!("expected Scala Structured import, got {other:?}"),
        }
    }

    /// Grammar fixture: lock the exact tree-sitter-scala node kinds this parser
    /// depends on. The grammar emits flat `import_declaration` children, selector
    /// braces as `namespace_selectors`, Scala 2 arrows as
    /// `arrow_renamed_identifier`, Scala 3 renames as `as_renamed_identifier`,
    /// and `_`/`*`/`given` through `namespace_wildcard`.
    #[test]
    fn scala_grammar_node_kinds_are_stable() {
        let src = "import a.b.C\nimport a.b._\nimport a.b.*\nimport a.b.{C, D}\nimport a.b.{C => D}\nimport a.b.C as D\nimport a.b.given\n\nobject Test { val x = 1 }\n";
        let (tree, _) = parse_scala(src);
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
            "import_declaration",
            "identifier",
            ".",
            "namespace_selectors",
            "namespace_wildcard",
            "arrow_renamed_identifier",
            "as_renamed_identifier",
            "=>",
            "as",
            "_",
            "*",
            "given",
        ] {
            assert!(
                kinds.contains(required),
                "scala grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_scala_supported_forms() {
        let (_, block) = parse_scala(
            "import a.b.C\nimport a.b._\nimport a.b.*\nimport a.b.{C, D}\nimport a.b.{C => D}\nimport a.b.C as D\nimport a.b.given\n\nobject Test { val x = 1 }\n",
        );
        assert_eq!(block.imports.len(), 7);

        assert_scala_import(&block.imports[0], "a.b.C", &[], None, &[], None);
        assert_scala_import(
            &block.imports[1],
            "a.b",
            &[],
            Some(SCALA_WILDCARD_FLAT_MARKER),
            &["wildcard"],
            None,
        );
        assert_scala_import(
            &block.imports[2],
            "a.b",
            &[],
            Some(SCALA_WILDCARD_FLAT_MARKER),
            &["wildcard"],
            None,
        );
        assert_scala_import(&block.imports[3], "a.b", &["C", "D"], None, &[], None);
        assert_scala_import(&block.imports[4], "a.b", &["C as D"], None, &[], None);
        assert_scala_import(&block.imports[5], "a.b", &["C as D"], None, &[], None);
        assert_scala_import(
            &block.imports[6],
            "a.b",
            &[],
            Some(SCALA_GIVEN_FLAT_MARKER),
            &[],
            Some(SCALA_GIVEN_FLAT_MARKER),
        );
    }

    fn assert_scala_import(
        imp: &ImportStatement,
        module_path: &str,
        names: &[&str],
        default_import: Option<&str>,
        modifiers: &[&str],
        import_kind: Option<&str>,
    ) {
        let expected_names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        let expected_modifiers: Vec<String> = modifiers
            .iter()
            .map(|modifier| modifier.to_string())
            .collect();

        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, expected_names);
        assert_eq!(imp.default_import.as_deref(), default_import);
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, classify_group_scala(module_path));

        let (structured_names, structured_modifiers, structured_import_kind) = structured(imp);
        assert_eq!(structured_names, expected_names.as_slice());
        assert_eq!(structured_modifiers, expected_modifiers.as_slice());
        assert_eq!(structured_import_kind, import_kind);
    }

    #[test]
    fn generate_scala_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest::legacy("a.b.C", &[], None, None, false)
            ),
            "import a.b.C"
        );

        let wildcard_modifiers = vec!["wildcard".to_string()];
        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest {
                    module_path: "a.b",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &wildcard_modifiers,
                    import_kind: None,
                }
            ),
            "import a.b.*"
        );

        let scala2_wildcard_modifiers =
            vec!["wildcard".to_string(), SCALA2_DIALECT_MODIFIER.to_string()];
        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest {
                    module_path: "a.b",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &scala2_wildcard_modifiers,
                    import_kind: None,
                }
            ),
            "import a.b._"
        );

        let grouped_names = vec!["C".to_string(), "D".to_string()];
        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest::legacy("a.b", &grouped_names, None, None, false)
            ),
            "import a.b.{C, D}"
        );

        let renamed_name = vec!["C => D".to_string()];
        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest::legacy("a.b", &renamed_name, None, None, false)
            ),
            "import a.b.C as D"
        );

        let scala2_modifiers = vec![SCALA2_DIALECT_MODIFIER.to_string()];
        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest {
                    module_path: "a.b",
                    names: &renamed_name,
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &scala2_modifiers,
                    import_kind: None,
                }
            ),
            "import a.b.{C => D}"
        );

        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest {
                    module_path: "a.b.C",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("D"),
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                }
            ),
            "import a.b.C as D"
        );

        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest {
                    module_path: "a.b",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some(SCALA_GIVEN_FLAT_MARKER),
                }
            ),
            "import a.b.given"
        );
    }

    #[test]
    fn scala_round_trips_supported_forms_with_scala3_generation_defaults() {
        for (src, expected) in [
            ("import a.b.C", "import a.b.C"),
            ("import a.b._", "import a.b.*"),
            ("import a.b.*", "import a.b.*"),
            ("import a.b.{C, D}", "import a.b.{C, D}"),
            ("import a.b.{C => D}", "import a.b.C as D"),
            ("import a.b.C as D", "import a.b.C as D"),
            ("import a.b.given", "import a.b.given"),
        ] {
            let (_, block) = parse_scala(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (alias, modifiers, import_kind) = match &imp.form {
                ImportForm::Structured {
                    alias,
                    modifiers,
                    import_kind,
                    ..
                } => (
                    alias.as_deref(),
                    modifiers.as_slice(),
                    import_kind.as_deref(),
                ),
                other => panic!("expected Scala Structured import, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Scala,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: imp.default_import.as_deref(),
                    namespace: None,
                    alias,
                    type_only: false,
                    modifiers,
                    import_kind,
                },
            );
            assert_eq!(regenerated, expected, "round-trip mismatch for {src:?}");
        }
    }

    #[test]
    fn scala_opaque_multi_import_is_preserved_as_raw_statement() {
        let (_, block) = parse_scala("import a.b.C, d.e.F\nobject O {}\n");
        assert_eq!(block.imports.len(), 1);
        assert_scala_import(
            &block.imports[0],
            "a.b.C, d.e.F",
            &[],
            None,
            &["raw-multi"],
            None,
        );
        assert_eq!(
            generate_import(
                LangId::Scala,
                &ImportRequest::legacy(&block.imports[0].module_path, &[], None, None, false)
            ),
            "import a.b.C, d.e.F"
        );
    }

    #[test]
    fn classify_scala_groups() {
        assert_eq!(
            classify_group_scala("scala.collection.Seq"),
            ImportGroup::Stdlib
        );
        assert_eq!(classify_group_scala("java.util.List"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_scala("javax.inject.Inject"),
            ImportGroup::Stdlib
        );
        assert_eq!(
            classify_group_scala("cats.effect.IO"),
            ImportGroup::External
        );
        assert_eq!(
            classify_group_scala("com.example.App"),
            ImportGroup::External
        );
    }
}
