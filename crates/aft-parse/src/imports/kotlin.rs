use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

pub fn classify_group_kotlin(module_path: &str) -> ImportGroup {
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

pub fn parse_kotlin_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    collect_kotlin_imports(source, root, &mut imports);
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

/// Collects the import headers under `root` in source order, without
/// recursion, so a deeply nested file cannot overflow the stack.
fn collect_kotlin_imports(source: &str, root: Node, imports: &mut Vec<ImportStatement>) {
    super::for_each_preorder_until(root, "import_header", |node| {
        if let Some(imp) = parse_kotlin_import_header(source, &node) {
            imports.push(imp);
        }
    });
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

pub fn generate_kotlin_import_line(req: &ImportRequest) -> String {
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
