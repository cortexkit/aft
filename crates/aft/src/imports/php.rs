pub use aft_parse::imports::php::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax, PhpImportClause,
};
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::{Node, Tree};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};

    fn parse_php(src: &str) -> (Tree, ImportBlock) {
        let g = grammar_for(LangId::Php).unwrap();
        let mut p = tree_sitter::Parser::new();
        p.set_language(&g).unwrap();
        let tree = p.parse(src, None).unwrap();
        let block = parse_imports(src, &tree, LangId::Php);
        (tree, block)
    }

    #[test]
    fn php_grammar_node_kinds_are_stable() {
        let src = "<?php\nuse App\\Foo;\nuse App\\Foo as Bar, App\\Other;\nuse function App\\helper, App\\other_helper;\nuse const App\\VERSION;\nuse App\\{Foo, Bar as Baz};\n";
        let (tree, _) = parse_php(src);
        let mut kinds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        fn walk(node: Node, kinds: &mut std::collections::BTreeSet<String>) {
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
            "namespace_use_declaration",
            "namespace_use_clause",
            "qualified_name",
            "namespace_name",
            "namespace_use_group",
            "name",
            "as",
            "function",
            "const",
            "\\",
        ] {
            assert!(
                kinds.contains(required),
                "php grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_php_all_supported_forms() {
        let (_, block) = parse_php(
            r"<?php
use App\Foo;
use App\Foo as Bar;
use function App\helper;
use const App\VERSION;
",
        );
        assert_eq!(block.imports.len(), 4);

        assert_php_import(&block.imports[0], r"App\Foo", None, None);
        assert_php_import(&block.imports[1], r"App\Foo", Some("Bar"), None);
        assert_php_import(&block.imports[2], r"App\helper", None, Some("function"));
        assert_php_import(&block.imports[3], r"App\VERSION", None, Some("const"));
    }

    fn collect_php_imports_full_walk(source: &str, node: Node, imports: &mut Vec<ImportStatement>) {
        if node.kind() == "namespace_use_declaration" {
            if let Some(imp) = parse_php_namespace_use_declaration(source, &node) {
                imports.push(imp);
            }
            return;
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                collect_php_imports_full_walk(source, cursor.node(), imports);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn count_tree_nodes(node: Node) -> usize {
        let mut count = 1;
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                count += count_tree_nodes(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        count
    }

    #[test]
    fn pruned_php_collector_matches_full_walk_across_namespace_shapes() {
        let sources = [
            "<?php\nuse GlobalApp\\Thing;\nclass C { function method() {} }\n",
            "<?php\nnamespace First;\nuse App\\One, App\\Two;\nclass C {}\n",
            "<?php\nnamespace Braced { use App\\One; class C {} use App\\Two; }\n",
            "<?php\nnamespace First { use App\\One; class A {} }\nnamespace Second { class B {} use App\\Two, App\\Three; }\n",
            "<?php\nuse App\\Before;\nclass A { function method() { return 1; } }\nuse App\\After;\nclass B {}\n",
        ];

        for source in sources {
            let (tree, pruned) = parse_php(source);
            let mut full_walk_imports = Vec::new();
            collect_php_imports_full_walk(source, tree.root_node(), &mut full_walk_imports);
            let full_walk = ImportBlock {
                byte_range: import_byte_range(&full_walk_imports),
                imports: full_walk_imports,
            };
            assert_eq!(pruned, full_walk, "collector mismatch for {source:?}");
        }
    }

    /// Namespace blocks nested thousands deep (error recovery accepts them)
    /// are collected on a small stack, in source order: the collector keeps
    /// its pending nodes on the heap, where a recursive one overflowed the
    /// stack and aborted the process.
    #[test]
    fn deeply_nested_namespace_blocks_collect_on_a_small_stack() {
        const DEPTH: usize = 3_000;
        let source = format!(
            "<?php\n{}use App\\Inner;{}\nuse App\\Outer;\n",
            "namespace A { ".repeat(DEPTH),
            " }".repeat(DEPTH)
        );
        let imports = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || parse_php(&source).1.imports)
            .unwrap()
            .join()
            .expect("the collector thread panicked");
        let modules: Vec<_> = imports.iter().map(|i| i.module_path.as_str()).collect();
        assert_eq!(modules, [r"App\Inner", r"App\Outer"]);
    }

    #[test]
    fn pruned_php_collector_does_not_visit_method_bodies() {
        let mut source = String::from("<?php\nuse App\\Keep;\nclass Huge {\n");
        for index in 0..2_000 {
            source.push_str(&format!(
                "function method{index}() {{ $value = {index}; return $value; }}\n"
            ));
        }
        source.push_str("}\n");

        let (tree, expected) = parse_php(&source);
        let mut imports = Vec::new();
        let pruned_visits = collect_php_imports_in_scope(&source, tree.root_node(), &mut imports);
        let full_walk_visits = count_tree_nodes(tree.root_node());

        assert_eq!(imports, expected.imports);
        assert!(
            pruned_visits <= 4,
            "collector visited {pruned_visits} nodes for three top-level declarations"
        );
        assert!(
            full_walk_visits > pruned_visits * 1_000,
            "tripwire fixture is not deep enough: full={full_walk_visits}, pruned={pruned_visits}"
        );
        eprintln!("PHP collector node visits: full={full_walk_visits}, pruned={pruned_visits}");
    }

    fn assert_php_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_alias: Option<&str>,
        expected_import_kind: Option<&str>,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.default_import, None);
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, ImportGroup::External);

        assert_eq!(
            imp.form,
            ImportForm::Php {
                clauses: vec![PhpImportClause {
                    module_path: module_path.to_string(),
                    alias: expected_alias.map(str::to_string),
                    import_kind: expected_import_kind.map(str::to_string),
                }],
            }
        );
    }

    #[test]
    fn parse_php_comma_separated_clauses_keeps_each_clause() {
        let (_, block) = parse_php(
            "<?php\nuse App\\First as One, App\\Second, App\\Third as Three;\nuse function App\\first, App\\second;\n",
        );
        assert_eq!(block.imports.len(), 2);
        assert_eq!(
            block.imports[0].form,
            ImportForm::Php {
                clauses: vec![
                    PhpImportClause {
                        module_path: "App\\First".to_string(),
                        alias: Some("One".to_string()),
                        import_kind: None,
                    },
                    PhpImportClause {
                        module_path: "App\\Second".to_string(),
                        alias: None,
                        import_kind: None,
                    },
                    PhpImportClause {
                        module_path: "App\\Third".to_string(),
                        alias: Some("Three".to_string()),
                        import_kind: None,
                    },
                ],
            }
        );
        assert_eq!(
            block.imports[1].form,
            ImportForm::Php {
                clauses: vec![
                    PhpImportClause {
                        module_path: "App\\first".to_string(),
                        alias: None,
                        import_kind: Some("function".to_string()),
                    },
                    PhpImportClause {
                        module_path: "App\\second".to_string(),
                        alias: None,
                        import_kind: Some("function".to_string()),
                    },
                ],
            }
        );
    }

    #[test]
    fn parse_php_grouped_use_is_captured_for_raw_preserving_organize() {
        let (_, block) = parse_php(
            "<?php\nuse App\\Alpha;\nuse App\\{Beta, Gamma as G};\nuse function App\\helper;\n",
        );
        assert_eq!(block.imports.len(), 3);
        assert_eq!(block.imports[1].module_path, "App");
        assert_eq!(block.imports[1].raw_text, "use App\\{Beta, Gamma as G};");
        assert_eq!(block.imports[1].names, vec!["Beta", "Gamma as G"]);
        assert_eq!(
            block.imports[1].form,
            ImportForm::Structured {
                named: vec!["Beta".to_string(), "Gamma as G".to_string()],
                namespace: None,
                alias: None,
                modifiers: vec!["group".to_string()],
                import_kind: None,
            }
        );
    }

    #[test]
    fn generate_php_all_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest::legacy("App\\Foo", &[], None, None, false)
            ),
            "use App\\Foo;"
        );
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: "App\\Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("Bar"),
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                }
            ),
            "use App\\Foo as Bar;"
        );
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: "App\\helper",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some("function"),
                }
            ),
            "use function App\\helper;"
        );
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: "App\\VERSION",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some("const"),
                }
            ),
            "use const App\\VERSION;"
        );
    }

    #[test]
    fn classify_group_php_always_external() {
        assert_eq!(classify_group_php("App\\Foo"), ImportGroup::External);
        assert_eq!(classify_group_php("\\App\\Foo"), ImportGroup::External);
        assert_eq!(classify_group_php("Vendor\\Package"), ImportGroup::External);
    }

    #[test]
    fn php_round_trips_through_parse_generate() {
        for src in [
            "use App\\Foo;",
            "use App\\Foo as Bar;",
            "use function App\\helper;",
            "use const App\\VERSION;",
        ] {
            let php_src = format!("<?php\n{src}\n");
            let (_, block) = parse_php(&php_src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (alias, import_kind) = match &imp.form {
                ImportForm::Php { clauses } => {
                    let clause = clauses.first().expect("single PHP import clause");
                    (clause.alias.as_deref(), clause.import_kind.as_deref())
                }
                other => panic!("expected PHP clause form, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: None,
                    namespace: None,
                    alias,
                    type_only: false,
                    modifiers: &[],
                    import_kind,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
