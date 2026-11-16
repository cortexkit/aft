use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax, PhpImportClause,
};
use tree_sitter::{Node, Tree};

pub fn classify_group_php(_module_path: &str) -> ImportGroup {
    ImportGroup::External
}

pub fn parse_php_imports(source: &str, tree: &Tree) -> ImportBlock {
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

/// Collects the `use` declarations of `scope` and of the namespace blocks in
/// it, in source order, and returns how many nodes it looked at. Only the
/// children of scopes and of namespaces are looked at, so the walk skips
/// function bodies and expressions.
///
/// The walk keeps its pending nodes on the heap rather than recursing:
/// namespace blocks can nest (error recovery accepts them) arbitrarily
/// deep, and a stack overflow aborts the process. Children are pushed in
/// reverse, so a nested namespace's imports still come before those of the
/// declarations after it.
fn collect_php_imports_in_scope(
    source: &str,
    scope: Node,
    imports: &mut Vec<ImportStatement>,
) -> usize {
    /// Where a pending node sits: directly in a scope, or directly in a
    /// namespace definition.
    #[derive(Clone, Copy)]
    enum Within {
        Scope,
        Namespace,
    }
    fn push_children<'tree>(
        pending: &mut Vec<(Node<'tree>, Within)>,
        node: Node<'tree>,
        within: Within,
    ) {
        let start = pending.len();
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor).map(|child| (child, within)));
        pending[start..].reverse();
    }

    let mut visited = 0;
    let mut pending = Vec::new();
    push_children(&mut pending, scope, Within::Scope);
    while let Some((child, within)) = pending.pop() {
        visited += 1;
        match (within, child.kind()) {
            (Within::Scope, "namespace_use_declaration") => {
                if let Some(imp) = parse_php_namespace_use_declaration(source, &child) {
                    imports.push(imp);
                }
            }
            (Within::Scope, "namespace_definition") => {
                push_children(&mut pending, child, Within::Namespace);
            }
            (Within::Namespace, "compound_statement") => {
                push_children(&mut pending, child, Within::Scope);
            }
            _ => {}
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

pub fn php_import_satisfies_request(imp: &ImportStatement, req: &ImportRequest<'_>) -> bool {
    let ImportForm::Php { clauses } = &imp.form else {
        return false;
    };

    clauses.iter().any(|clause| {
        clause.module_path == req.module_path
            && clause.alias.as_deref() == req.alias
            && clause.import_kind.as_deref() == req.import_kind
    })
}

pub fn php_import_matches_module(imp: &ImportStatement, module: &str) -> bool {
    match &imp.form {
        ImportForm::Php { clauses } => clauses.iter().any(|clause| clause.module_path == module),
        _ => imp.module_path == module,
    }
}

pub fn rewrite_php_import_without_module(
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

pub fn php_grouped_use_shares_prefix(imp: &ImportStatement, module: &str) -> bool {
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

pub fn php_grouped_use_matches_module(imp: &ImportStatement, module: &str) -> bool {
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

pub fn generate_php_import_line(req: &ImportRequest) -> String {
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
