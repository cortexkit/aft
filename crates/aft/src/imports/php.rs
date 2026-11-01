use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax, PhpImportClause,
};
use tree_sitter::{Node, Tree};

pub(crate) fn classify_group_php(_module_path: &str) -> ImportGroup {
    ImportGroup::External
}

pub(crate) fn parse_php_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    collect_php_imports(source, root, &mut imports);
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn collect_php_imports(source: &str, root: Node, imports: &mut Vec<ImportStatement>) {
    collect_php_imports_in_scope(source, root, imports);
}

fn collect_php_imports_in_scope(
    source: &str,
    scope: Node,
    imports: &mut Vec<ImportStatement>,
) -> usize {
    let mut visited = 0;
    let mut cursor = scope.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            visited += 1;
            match child.kind() {
                "namespace_use_declaration" => {
                    if let Some(imp) = parse_php_namespace_use_declaration(source, &child) {
                        imports.push(imp);
                    }
                }
                "namespace_definition" => {
                    visited += collect_php_imports_in_namespace(source, child, imports);
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    visited
}

fn collect_php_imports_in_namespace(
    source: &str,
    namespace: Node,
    imports: &mut Vec<ImportStatement>,
) -> usize {
    let mut visited = 0;
    let mut cursor = namespace.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            visited += 1;
            if child.kind() == "compound_statement" {
                visited += collect_php_imports_in_scope(source, child, imports);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    visited
}

fn parse_php_namespace_use_declaration(source: &str, node: &Node) -> Option<ImportStatement> {
    let clauses = direct_children(node, "namespace_use_clause");
    if !clauses.is_empty() {
        return parse_php_clause_declaration(source, node, &clauses);
    }

    if find_direct_child(node, "namespace_use_group").is_some() {
        return parse_php_grouped_namespace_use_declaration(source, node);
    }

    parse_php_payload_namespace_use_declaration(source, node)
}

fn parse_php_clause_declaration(
    source: &str,
    node: &Node,
    clause_nodes: &[Node<'_>],
) -> Option<ImportStatement> {
    let mut clauses: Vec<PhpImportClause> = clause_nodes
        .iter()
        .filter_map(|clause| {
            let (module_path, alias, import_kind) = parse_php_namespace_use_clause(source, clause)?;
            Some(PhpImportClause {
                module_path,
                alias,
                import_kind,
            })
        })
        .collect();
    if clauses.is_empty() {
        return None;
    }

    // In `use function A, B;` and `use const A, B;`, PHP writes the kind once
    // for the whole declaration. The grammar attaches it to one clause, so copy
    // it to siblings to keep every clause semantically complete.
    let declaration_kind = clauses
        .first()
        .and_then(|clause| clause.import_kind.clone());
    if let Some(kind) = declaration_kind {
        for clause in &mut clauses {
            clause.import_kind.get_or_insert_with(|| kind.clone());
        }
    }

    let module_path = clauses.first()?.module_path.clone();
    let group = classify_group_php(&module_path);

    Some(ImportStatement {
        module_path,
        names: Vec::new(),
        default_import: None,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range: node.byte_range(),
        raw_text: source[node.byte_range()].to_string(),
        form: ImportForm::Php { clauses },
    })
}

fn parse_php_payload_namespace_use_declaration(
    source: &str,
    node: &Node,
) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();
    let mut payload = php_use_payload(source, node)?;
    let mut import_kind = None;

    for kind in ["function", "const"] {
        if let Some(rest) = payload
            .strip_prefix(kind)
            .and_then(|rest| rest.strip_prefix(' '))
        {
            import_kind = Some(kind.to_string());
            payload = rest.trim().to_string();
            break;
        }
    }

    let (module_path, alias) = if let Some((path, alias)) = payload.split_once(" as ") {
        (path.trim().to_string(), Some(alias.trim().to_string()))
    } else {
        (payload.trim().to_string(), None)
    };
    if module_path.is_empty() {
        return None;
    }

    let group = classify_group_php(&module_path);

    Some(ImportStatement {
        module_path,
        names: Vec::new(),
        default_import: None,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: Vec::new(),
            namespace: None,
            alias,
            modifiers: vec![],
            import_kind,
        },
    })
}

fn parse_php_grouped_namespace_use_declaration(
    source: &str,
    node: &Node,
) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();
    let (module_path, import_kind) = parse_php_grouped_use_header(source, node)
        .or_else(|| php_use_payload(source, node).map(|payload| (payload, None)))?;
    if module_path.is_empty() {
        return None;
    }

    let names = parse_php_grouped_use_members(&raw_text);
    let group = classify_group_php(&module_path);

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
            modifiers: vec!["group".to_string()],
            import_kind,
        },
    })
}

fn parse_php_grouped_use_members(raw_text: &str) -> Vec<String> {
    let Some(open) = raw_text.find('{') else {
        return Vec::new();
    };
    let Some(close) = raw_text.rfind('}') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }

    raw_text[open + 1..close]
        .split(',')
        .map(str::trim)
        .filter(|member| !member.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_php_grouped_use_header(source: &str, node: &Node) -> Option<(String, Option<String>)> {
    let mut module_path: Option<String> = None;
    let mut import_kind: Option<String> = None;
    let mut leading_absolute = false;

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "namespace_use_group" {
                break;
            }

            let text = source[child.byte_range()].trim();
            match child.kind() {
                "function" | "const" => import_kind = Some(text.to_string()),
                "\\" if module_path.is_none() => leading_absolute = true,
                "qualified_name" | "namespace_name" | "name" => {
                    if module_path.is_none() {
                        module_path = Some(text.to_string());
                    }
                }
                _ => {}
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let mut module_path = module_path?;
    if leading_absolute && !module_path.starts_with('\\') {
        module_path.insert(0, '\\');
    }

    Some((module_path, import_kind))
}

fn php_use_payload(source: &str, node: &Node) -> Option<String> {
    let raw = source[node.byte_range()].trim();
    raw.strip_prefix("use")
        .map(str::trim)
        .map(|payload| payload.strip_suffix(';').map(str::trim).unwrap_or(payload))
        .map(str::to_string)
        .filter(|payload| !payload.is_empty())
}

fn parse_php_namespace_use_clause(
    source: &str,
    node: &Node,
) -> Option<(String, Option<String>, Option<String>)> {
    let mut module_path: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut import_kind: Option<String> = None;
    let mut saw_as = false;
    let mut leading_absolute = false;

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let text = source[child.byte_range()].trim();
            match child.kind() {
                "function" | "const" => import_kind = Some(text.to_string()),
                "\\" if module_path.is_none() => leading_absolute = true,
                "qualified_name" => {
                    if module_path.is_none() {
                        module_path = Some(text.to_string());
                    }
                }
                "name" => {
                    if saw_as {
                        alias = Some(text.to_string());
                    } else if module_path.is_none() {
                        module_path = Some(text.to_string());
                    }
                }
                "as" => saw_as = true,
                _ => {}
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let mut module_path = module_path?;
    if leading_absolute && !module_path.starts_with('\\') {
        module_path.insert(0, '\\');
    }
    if module_path.is_empty() {
        return None;
    }

    Some((module_path, alias, import_kind))
}

pub(crate) fn php_import_satisfies_request(imp: &ImportStatement, req: &ImportRequest<'_>) -> bool {
    let ImportForm::Php { clauses } = &imp.form else {
        return false;
    };

    clauses.iter().any(|clause| {
        clause.module_path == req.module_path
            && clause.alias.as_deref() == req.alias
            && clause.import_kind.as_deref() == req.import_kind
    })
}

pub(crate) fn php_import_matches_module(imp: &ImportStatement, module: &str) -> bool {
    match &imp.form {
        ImportForm::Php { clauses } => clauses.iter().any(|clause| clause.module_path == module),
        _ => imp.module_path == module,
    }
}

pub(crate) fn rewrite_php_import_without_module(
    imp: &ImportStatement,
    module: &str,
) -> Option<Option<String>> {
    let ImportForm::Php { clauses } = &imp.form else {
        return None;
    };
    let remaining: Vec<&PhpImportClause> = clauses
        .iter()
        .filter(|clause| clause.module_path != module)
        .collect();
    if remaining.len() == clauses.len() {
        return None;
    }
    if remaining.is_empty() {
        return Some(None);
    }

    let shared_kind = remaining.first().and_then(|first| {
        first.import_kind.as_deref().filter(|kind| {
            remaining
                .iter()
                .all(|clause| clause.import_kind.as_deref() == Some(*kind))
        })
    });
    let body = remaining
        .iter()
        .map(|clause| {
            let kind = clause
                .import_kind
                .as_deref()
                .filter(|kind| Some(*kind) != shared_kind)
                .map(|kind| format!("{kind} "))
                .unwrap_or_default();
            let alias = clause
                .alias
                .as_deref()
                .map(|alias| format!(" as {alias}"))
                .unwrap_or_default();
            format!("{kind}{}{alias}", clause.module_path)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let kind_prefix = shared_kind
        .map(|kind| format!("{kind} "))
        .unwrap_or_default();
    Some(Some(format!("use {kind_prefix}{body};")))
}

pub(crate) fn php_grouped_use_shares_prefix(imp: &ImportStatement, module: &str) -> bool {
    if !php_import_is_grouped(imp) {
        return false;
    }

    let prefix = imp.module_path.trim_matches('\\');
    let module = module.trim_matches('\\');
    module == prefix
        || module
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('\\'))
}

pub(crate) fn php_grouped_use_matches_module(imp: &ImportStatement, module: &str) -> bool {
    if !php_grouped_use_shares_prefix(imp, module) {
        return false;
    }

    let prefix = imp.module_path.trim_matches('\\');
    let module = module.trim_matches('\\');
    if module == prefix {
        return true;
    }

    let member = module
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('\\'))
        .unwrap_or(module);
    imp.names
        .iter()
        .any(|name| super::specifier_matches(name, member))
}

fn php_import_is_grouped(imp: &ImportStatement) -> bool {
    matches!(
        &imp.form,
        ImportForm::Structured { modifiers, .. } if modifiers.iter().any(|modifier| modifier == "group")
    )
}

fn direct_children<'tree>(node: &Node<'tree>, kind: &str) -> Vec<Node<'tree>> {
    let mut children = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind {
                children.push(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    children
}

fn find_direct_child<'tree>(node: &Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    direct_children(node, kind).into_iter().next()
}

pub(crate) fn generate_php_import_line(req: &ImportRequest) -> String {
    let mut line = String::from("use ");
    if let Some(kind) = req.import_kind {
        if !kind.is_empty() {
            line.push_str(kind);
            line.push(' ');
        }
    }
    line.push_str(req.module_path);
    if let Some(alias) = req.alias {
        if !alias.is_empty() {
            line.push_str(" as ");
            line.push_str(alias);
        }
    }
    line.push(';');
    line
}

pub struct PhpSyntax;

impl ImportSyntax for PhpSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_php_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_php_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_php(module_path)
    }
}

pub static PHP_SYNTAX: PhpSyntax = PhpSyntax;

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
