use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

pub(crate) fn classify_group_java(module_path: &str) -> ImportGroup {
    if module_path.starts_with("java.") || module_path.starts_with("javax.") {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

pub(crate) fn parse_java_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "import_declaration" {
                if let Some(imp) = parse_java_import_declaration(source, &node) {
                    imports.push(imp);
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn parse_java_import_declaration(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();
    let mut module_path: Option<String> = None;
    let mut modifiers = Vec::new();

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "scoped_identifier" => {
                    module_path = Some(source[child.byte_range()].to_string());
                }
                "static" => modifiers.push("static".to_string()),
                "asterisk" => modifiers.push("wildcard".to_string()),
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let mut module_path = module_path?;
    if module_path.is_empty() {
        return None;
    }

    // Java static member imports use the same module-plus-names model accepted
    // by add_import. Wildcards stay attached to the declaring type because `*`
    // is a statement modifier rather than a removable member binding.
    let mut names = Vec::new();
    let is_static = modifiers.iter().any(|modifier| modifier == "static");
    let is_wildcard = modifiers.iter().any(|modifier| modifier == "wildcard");
    if is_static && !is_wildcard {
        if let Some((declaring_type, member)) = module_path.rsplit_once('.') {
            names.push(member.to_string());
            module_path = declaring_type.to_string();
        }
    }

    let group = classify_group_java(&module_path);

    Some(ImportStatement {
        module_path,
        names: names.clone(),
        default_import: None,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: names,
            namespace: None,
            alias: None,
            modifiers,
            import_kind: None,
        },
    })
}

pub(crate) fn generate_java_import_line(req: &ImportRequest) -> String {
    let is_static = req.modifiers.iter().any(|m| m == "static");
    let static_prefix = if is_static { "static " } else { "" };
    let is_wildcard = req.modifiers.iter().any(|m| m == "wildcard");
    let wildcard_suffix = if is_wildcard { ".*" } else { "" };

    // A static member import targets a specific member: `import static Class.member;`.
    // Java has no grouped import form, so multiple members each need their own
    // statement (newline-joined). With one name this is identical to the old
    // single-name path; with several it avoids generating the invalid
    // `import static Class;` (which drops every requested member).
    if is_static && !is_wildcard && !req.names.is_empty() {
        return req
            .names
            .iter()
            .map(|name| format!("import static {}.{};", req.module_path, name))
            .collect::<Vec<_>>()
            .join("\n");
    }

    // Remaining shapes: non-static `import pkg.Class;`, static wildcard
    // `import static pkg.Class.*;`, and the static-member form where the member
    // is already part of `module_path` (names empty).
    format!(
        "import {static_prefix}{}{wildcard_suffix};",
        req.module_path
    )
}

pub struct JavaSyntax;

impl ImportSyntax for JavaSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_java_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_java_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_java(module_path)
    }
}

pub static JAVA_SYNTAX: JavaSyntax = JavaSyntax;

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
