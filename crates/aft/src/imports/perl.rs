pub use aft_parse::imports::perl::*;

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

    fn parse_perl(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Perl).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Perl);
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
            other => panic!("expected Perl Structured import, got {other:?}"),
        }
    }

    /// Grammar fixture: lock the exact tree-sitter-perl node kinds this parser
    /// depends on. This grammar represents every `use`/`no` pragma (plain or
    /// specialized like `use parent` / `use constant`) as a single
    /// `use_statement` with a `module` field and a leading `use`/`no` keyword
    /// token, and runtime `require` as an `expression_statement` wrapping a
    /// `require_expression`.
    #[test]
    fn perl_grammar_node_kinds_are_stable() {
        let src = "use Foo::Bar;\nuse Foo qw(a b);\nuse parent -norequire, 'Base';\nuse constant PI => 3.14;\nrequire Foo;\nno warnings;\nno strict 'refs';\n";
        let (tree, _) = parse_perl(src);
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
            "source_file",
            "use_statement",
            "expression_statement",
            "require_expression",
            "package",
            "use",
            "no",
            ";",
        ] {
            assert!(
                kinds.contains(required),
                "perl grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_perl_supported_forms() {
        let (_, block) = parse_perl(
            "use Foo::Bar;\nuse Foo qw(a b);\nuse parent -norequire, 'Base';\nuse constant PI => 3.14;\nrequire Foo;\nno warnings;\nno strict 'refs';\n",
        );
        assert_eq!(block.imports.len(), 7);

        assert_perl_import(&block.imports[0], "Foo::Bar", PERL_USE_KIND, "");
        assert_perl_import(&block.imports[1], "Foo", PERL_USE_KIND, "qw(a b)");
        assert_perl_import(
            &block.imports[2],
            "parent",
            PERL_USE_KIND,
            "-norequire, 'Base'",
        );
        assert_perl_import(&block.imports[3], "constant", PERL_USE_KIND, "PI => 3.14");
        assert_perl_import(&block.imports[4], "Foo", PERL_REQUIRE_KIND, "");
        assert_perl_import(&block.imports[5], "warnings", PERL_NO_KIND, "");
        assert_perl_import(&block.imports[6], "strict", PERL_NO_KIND, "'refs'");
    }

    fn assert_perl_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_import_kind: &str,
        expected_raw_args: &str,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert!(imp.default_import.is_some());
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::SideEffect);
        assert_eq!(imp.group, ImportGroup::External);

        let marker = imp.default_import.as_deref().unwrap();
        assert_eq!(
            perl_marker_parts(marker),
            Some((expected_import_kind, expected_raw_args))
        );

        let (modifiers, import_kind) = structured(imp);
        assert_eq!(import_kind, Some(expected_import_kind));
        if expected_raw_args.is_empty() {
            assert!(modifiers.is_empty());
        } else {
            assert_eq!(modifiers, &[expected_raw_args.to_string()]);
        }
    }

    #[test]
    fn generate_perl_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy("Foo::Bar", &[], None, None, false)
            ),
            "use Foo::Bar;"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["qw(a b)".to_string()],
                    import_kind: Some(PERL_USE_KIND),
                }
            ),
            "use Foo qw(a b);"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some(PERL_REQUIRE_KIND),
                }
            ),
            "require Foo;"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "strict",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["'refs'".to_string()],
                    import_kind: Some(PERL_NO_KIND),
                }
            ),
            "no strict 'refs';"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "parent",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["-norequire, 'Base'".to_string()],
                    import_kind: Some(PERL_USE_KIND),
                }
            ),
            "use parent -norequire, 'Base';"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "constant",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["PI => 3.14".to_string()],
                    import_kind: Some(PERL_USE_KIND),
                }
            ),
            "use constant PI => 3.14;"
        );
    }

    #[test]
    fn generate_perl_preserves_organized_flat_markers() {
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy(
                    "Foo",
                    &[],
                    Some(&perl_flat_marker(PERL_REQUIRE_KIND, "")),
                    None,
                    false,
                )
            ),
            "require Foo;"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy(
                    "strict",
                    &[],
                    Some(&perl_flat_marker(PERL_NO_KIND, "'refs'")),
                    None,
                    false,
                )
            ),
            "no strict 'refs';"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy(
                    "Foo",
                    &[],
                    Some(&perl_flat_marker(PERL_USE_KIND, "qw(a b)")),
                    None,
                    false,
                )
            ),
            "use Foo qw(a b);"
        );
    }

    #[test]
    fn classify_group_perl_is_neutral_external() {
        assert_eq!(classify_group_perl("strict"), ImportGroup::External);
        assert_eq!(classify_group_perl("warnings"), ImportGroup::External);
        assert_eq!(classify_group_perl("Foo::Bar"), ImportGroup::External);
    }

    #[test]
    fn perl_round_trips_through_parse_generate() {
        for src in [
            "use Foo::Bar;",
            "use Foo qw(a b);",
            "use parent -norequire, 'Base';",
            "use constant PI => 3.14;",
            "require Foo;",
            "no warnings;",
            "no strict 'refs';",
        ] {
            let (_, block) = parse_perl(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (modifiers, import_kind) = structured(imp);
            let regenerated = generate_import(
                LangId::Perl,
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
