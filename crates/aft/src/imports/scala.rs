use super::{
    import_byte_range, sort_named_specifiers, ImportBlock, ImportForm, ImportGroup, ImportKind,
    ImportRequest, ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

const SCALA_WILDCARD_FLAT_MARKER: &str = "*";
const SCALA_GIVEN_FLAT_MARKER: &str = "given";
const SCALA2_DIALECT_MODIFIER: &str = "scala2";

pub(crate) fn classify_group_scala(module_path: &str) -> ImportGroup {
    if module_path == "scala"
        || module_path.starts_with("scala.")
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

pub(crate) fn scala_block_uses_scala2_dialect(block: &ImportBlock) -> bool {
    block.imports.iter().any(|imp| {
        imp.raw_text.contains("=>")
            || imp.raw_text.contains("._")
            || imp.names.iter().any(|name| name.trim() == "_")
    })
}

pub(crate) fn parse_scala_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "import_declaration" {
                if let Some(imp) = parse_scala_import_declaration(source, &node) {
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

fn parse_scala_import_declaration(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    let import_token = find_direct_child(node, "import")?;
    let mut special_child: Option<Node> = None;
    let mut has_top_level_comma = false;

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "namespace_selectors" | "namespace_wildcard" | "as_renamed_identifier" => {
                    special_child.get_or_insert(child);
                }
                // Scala also permits `import a.b.C, d.e.F`. Because one tree-sitter
                // node owns the whole comma-separated statement, splitting it into
                // multiple ImportStatements would give several imports the same byte
                // range and corrupt remove/organize edits. Keep it as an opaque raw
                // statement instead; adding/removing an individual constituent is an
                // explicitly unsupported sub-form.
                "," => has_top_level_comma = true,
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let mut names = Vec::new();
    let mut default_import = None;
    let mut modifiers = Vec::new();
    let mut import_kind = None;

    let module_path = if has_top_level_comma {
        modifiers.push("raw-multi".to_string());
        source[import_token.end_byte()..node.end_byte()]
            .trim()
            .to_string()
    } else if let Some(child) = special_child {
        match child.kind() {
            "namespace_selectors" => {
                names = parse_scala_namespace_selectors(source, &child);
                prefix_before_child(source, import_token, child)
            }
            "namespace_wildcard" => {
                let wildcard = source[child.byte_range()].trim();
                if wildcard == SCALA_GIVEN_FLAT_MARKER {
                    default_import = Some(SCALA_GIVEN_FLAT_MARKER.to_string());
                    import_kind = Some(SCALA_GIVEN_FLAT_MARKER.to_string());
                } else {
                    default_import = Some(SCALA_WILDCARD_FLAT_MARKER.to_string());
                    modifiers.push("wildcard".to_string());
                }
                prefix_before_child(source, import_token, child)
            }
            "as_renamed_identifier" => {
                if let Some(name) = parse_scala_renamed_identifier(source, &child) {
                    names.push(name);
                }
                prefix_before_child(source, import_token, child)
            }
            _ => return None,
        }
    } else {
        source[import_token.end_byte()..node.end_byte()]
            .trim()
            .to_string()
    };

    if module_path.is_empty() {
        return None;
    }

    let group = classify_group_scala(&module_path);

    Some(ImportStatement {
        module_path,
        names: names.clone(),
        default_import: default_import.clone(),
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
            import_kind,
        },
    })
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

fn prefix_before_child(source: &str, import_token: Node, child: Node) -> String {
    source[import_token.end_byte()..child.start_byte()]
        .trim()
        .trim_end_matches('.')
        .trim_end()
        .to_string()
}

fn parse_scala_namespace_selectors(source: &str, node: &Node) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "identifier" | "namespace_wildcard" => {
                    names.push(source[child.byte_range()].trim().to_string());
                }
                "as_renamed_identifier" | "arrow_renamed_identifier" => {
                    if let Some(name) = parse_scala_renamed_identifier(source, &child) {
                        names.push(name);
                    }
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    names
}

fn parse_scala_renamed_identifier(source: &str, node: &Node) -> Option<String> {
    let mut identifiers = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "identifier" || child.kind() == "namespace_wildcard" {
                identifiers.push(source[child.byte_range()].trim().to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    match identifiers.as_slice() {
        [from, to, ..] if !from.is_empty() && !to.is_empty() => Some(format!("{from} as {to}")),
        [single] if !single.is_empty() => Some(single.clone()),
        _ => None,
    }
}

pub(crate) fn generate_scala_import_line(req: &ImportRequest) -> String {
    let scala2 = req
        .modifiers
        .iter()
        .any(|modifier| modifier == SCALA2_DIALECT_MODIFIER);

    if is_scala_given_request(req) {
        return format!(
            "import {}.given",
            strip_scala_special_suffix(req.module_path)
        );
    }

    if is_scala_wildcard_request(req) {
        let wildcard = if scala2 { "_" } else { "*" };
        return format!(
            "import {}.{wildcard}",
            strip_scala_special_suffix(req.module_path)
        );
    }

    if let Some(alias) = req.alias.filter(|alias| !alias.is_empty()) {
        if scala2 {
            if let Some((prefix, leaf)) = req.module_path.rsplit_once('.') {
                return format!("import {}.{{{} => {}}}", prefix, leaf, alias);
            }
        }
        return format!("import {} as {}", req.module_path, alias);
    }

    if req.names.is_empty() {
        return format!("import {}", req.module_path);
    }

    let mut names: Vec<String> = req
        .names
        .iter()
        .map(|name| normalize_scala_rename_for_dialect(name, scala2))
        .filter(|name| !name.is_empty())
        .collect();
    sort_named_specifiers(&mut names);

    match names.as_slice() {
        [] => format!("import {}", req.module_path),
        [name] if scala2 && scala_selector_is_rename(name) => {
            format!("import {}.{{{}}}", req.module_path, name)
        }
        [name] => format!("import {}.{}", req.module_path, name),
        _ => format!("import {}.{{{}}}", req.module_path, names.join(", ")),
    }
}

fn is_scala_given_request(req: &ImportRequest) -> bool {
    req.import_kind == Some(SCALA_GIVEN_FLAT_MARKER)
        || req.default_import == Some(SCALA_GIVEN_FLAT_MARKER)
        || req.module_path.ends_with(".given")
}

fn is_scala_wildcard_request(req: &ImportRequest) -> bool {
    req.modifiers.iter().any(|modifier| modifier == "wildcard")
        || matches!(req.default_import, Some("*") | Some("_"))
        || matches!(req.namespace, Some("*") | Some("_"))
        || req.module_path.ends_with(".*")
        || req.module_path.ends_with("._")
}

fn strip_scala_special_suffix(module_path: &str) -> &str {
    module_path
        .strip_suffix(".given")
        .or_else(|| module_path.strip_suffix(".*"))
        .or_else(|| module_path.strip_suffix("._"))
        .unwrap_or(module_path)
}

fn normalize_scala_rename_for_dialect(name: &str, scala2: bool) -> String {
    let trimmed = name.trim();
    if scala2 {
        if let Some((from, to)) = trimmed.split_once("=>") {
            format!("{} => {}", from.trim(), to.trim())
        } else if let Some((from, to)) = trimmed.split_once(" as ") {
            format!("{} => {}", from.trim(), to.trim())
        } else if trimmed == "*" {
            "_".to_string()
        } else {
            trimmed.to_string()
        }
    } else if let Some((from, to)) = trimmed.split_once("=>") {
        format!("{} as {}", from.trim(), to.trim())
    } else {
        trimmed.to_string()
    }
}

fn scala_selector_is_rename(name: &str) -> bool {
    name.contains("=>") || name.contains(" as ")
}

pub struct ScalaSyntax;

impl ImportSyntax for ScalaSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_scala_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_scala_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_scala(module_path)
    }
}

pub static SCALA_SYNTAX: ScalaSyntax = ScalaSyntax;

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
