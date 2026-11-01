use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

pub(crate) fn classify_group_kotlin(module_path: &str) -> ImportGroup {
    if module_path == "kotlin"
        || module_path.starts_with("kotlin.")
        || module_path == "java"
        || module_path.starts_with("java.")
        || module_path == "javax"
        || module_path.starts_with("javax.")
    {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

pub(crate) fn parse_kotlin_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    collect_kotlin_imports(source, root, &mut imports);
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn collect_kotlin_imports(source: &str, node: Node, imports: &mut Vec<ImportStatement>) {
    if node.kind() == "import_header" {
        if let Some(imp) = parse_kotlin_import_header(source, &node) {
            imports.push(imp);
        }
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_kotlin_imports(source, cursor.node(), imports);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn parse_kotlin_import_header(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();
    let module_path = direct_child_text(source, node, "identifier")?;
    if module_path.is_empty() {
        return None;
    }

    let wildcard = find_direct_child(node, "wildcard_import").is_some();
    let alias = find_direct_child(node, "import_alias")
        .and_then(|alias_node| extract_kotlin_alias(source, &alias_node));
    let mut modifiers = Vec::new();
    if wildcard {
        modifiers.push("wildcard".to_string());
    }

    let group = classify_group_kotlin(&module_path);
    let default_import = if wildcard {
        Some("*".to_string())
    } else {
        alias.clone()
    };

    Some(ImportStatement {
        module_path,
        names: Vec::new(),
        default_import,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: Vec::new(),
            namespace: None,
            alias,
            modifiers,
            import_kind: None,
        },
    })
}

fn direct_child_text(source: &str, node: &Node, kind: &str) -> Option<String> {
    find_direct_child(node, kind)
        .map(|child| source[child.byte_range()].trim().to_string())
        .filter(|text| !text.is_empty())
}

fn find_direct_child<'tree>(node: &Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn extract_kotlin_alias(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if matches!(
                child.kind(),
                "type_identifier" | "simple_identifier" | "identifier"
            ) {
                let alias = source[child.byte_range()].trim().to_string();
                if !alias.is_empty() {
                    return Some(alias);
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    source[node.byte_range()]
        .trim()
        .strip_prefix("as")
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .map(str::to_string)
}

pub(crate) fn generate_kotlin_import_line(req: &ImportRequest) -> String {
    let default_import = req.default_import.filter(|value| *value != "*");
    let has_wildcard = req.modifiers.iter().any(|modifier| modifier == "wildcard")
        || req.default_import == Some("*")
        || req.module_path.ends_with(".*");
    let module_path = req
        .module_path
        .strip_suffix(".*")
        .unwrap_or(req.module_path);

    if has_wildcard {
        format!("import {module_path}.*")
    } else if let Some(alias) = req.alias.or(default_import) {
        format!("import {module_path} as {alias}")
    } else {
        format!("import {module_path}")
    }
}

pub struct KotlinSyntax;

impl ImportSyntax for KotlinSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_kotlin_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_kotlin_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_kotlin(module_path)
    }
}

pub static KOTLIN_SYNTAX: KotlinSyntax = KotlinSyntax;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_kotlin(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Kotlin).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Kotlin);
        (tree, block)
    }

    /// Grammar fixture: lock the tree-sitter-kotlin node kinds this parser uses.
    /// The current grammar emits an `import_list` containing `import_header`
    /// nodes with direct `identifier`, optional `wildcard_import`, and optional
    /// `import_alias` children.
    #[test]
    fn kotlin_grammar_node_kinds_are_stable() {
        let src = "package com.example\n\nimport kotlin.collections.List\nimport kotlin.math.*\nimport com.example.Foo as Bar\n\nclass C\n";
        let (tree, _) = parse_kotlin(src);
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
            "import_list",
            "import_header",
            "import",
            "identifier",
            "simple_identifier",
            "wildcard_import",
            "import_alias",
            "as",
            "type_identifier",
        ] {
            assert!(
                kinds.contains(required),
                "kotlin grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_kotlin_all_supported_forms() {
        let (_, block) = parse_kotlin(
            "package com.example\n\nimport kotlin.collections.List\nimport kotlin.math.*\nimport com.example.Foo as Bar\n\nclass C\n",
        );
        assert_eq!(block.imports.len(), 3);

        assert_kotlin_import(&block.imports[0], "kotlin.collections.List", None, &[]);
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);

        assert_kotlin_import(&block.imports[1], "kotlin.math", None, &["wildcard"]);
        assert_eq!(block.imports[1].default_import.as_deref(), Some("*"));
        assert_eq!(block.imports[1].group, ImportGroup::Stdlib);

        assert_kotlin_import(&block.imports[2], "com.example.Foo", Some("Bar"), &[]);
        assert_eq!(block.imports[2].default_import.as_deref(), Some("Bar"));
        assert_eq!(block.imports[2].group, ImportGroup::External);
    }

    fn assert_kotlin_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_alias: Option<&str>,
        expected_modifiers: &[&str],
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, classify_group_kotlin(module_path));

        assert_eq!(
            imp.form,
            ImportForm::Structured {
                named: vec![],
                namespace: None,
                alias: expected_alias.map(str::to_string),
                modifiers: expected_modifiers
                    .iter()
                    .map(|modifier| modifier.to_string())
                    .collect(),
                import_kind: None,
            }
        );
    }

    #[test]
    fn generate_kotlin_all_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Kotlin,
                &ImportRequest::legacy("kotlin.collections.List", &[], None, None, false)
            ),
            "import kotlin.collections.List"
        );

        let wildcard_modifiers = vec!["wildcard".to_string()];
        assert_eq!(
            generate_import(
                LangId::Kotlin,
                &ImportRequest {
                    module_path: "kotlin.math",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &wildcard_modifiers,
                    import_kind: None,
                }
            ),
            "import kotlin.math.*"
        );

        assert_eq!(
            generate_import(
                LangId::Kotlin,
                &ImportRequest {
                    module_path: "com.example.Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("Bar"),
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                }
            ),
            "import com.example.Foo as Bar"
        );
    }

    #[test]
    fn generate_kotlin_preserves_organized_wildcard_and_alias_fallbacks() {
        assert_eq!(
            generate_import(
                LangId::Kotlin,
                &ImportRequest::legacy("kotlin.math", &[], Some("*"), None, false)
            ),
            "import kotlin.math.*"
        );
        assert_eq!(
            generate_import(
                LangId::Kotlin,
                &ImportRequest::legacy("com.example.Foo", &[], Some("Bar"), None, false)
            ),
            "import com.example.Foo as Bar"
        );
    }

    #[test]
    fn classify_group_kotlin_stdlib_vs_external() {
        assert_eq!(
            classify_group_kotlin("kotlin.collections.List"),
            ImportGroup::Stdlib
        );
        assert_eq!(classify_group_kotlin("java.util.List"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_kotlin("javax.inject.Inject"),
            ImportGroup::Stdlib
        );
        assert_eq!(
            classify_group_kotlin("com.example.Foo"),
            ImportGroup::External
        );
        assert_eq!(
            classify_group_kotlin("org.example.Foo"),
            ImportGroup::External
        );
    }

    #[test]
    fn kotlin_round_trips_through_parse_generate() {
        for src in [
            "import kotlin.collections.List",
            "import kotlin.math.*",
            "import com.example.Foo as Bar",
        ] {
            let (_, block) = parse_kotlin(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (alias, modifiers) = match &imp.form {
                ImportForm::Structured {
                    alias, modifiers, ..
                } => (alias.as_deref(), modifiers.as_slice()),
                other => panic!("expected Kotlin Structured import, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Kotlin,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: None,
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
