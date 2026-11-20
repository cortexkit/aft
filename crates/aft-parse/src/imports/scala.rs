use super::{
    import_byte_range, sort_named_specifiers, ImportBlock, ImportForm, ImportGroup, ImportKind,
    ImportRequest, ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

test_support_visibility! {
const SCALA_WILDCARD_FLAT_MARKER: &str = "*";
}
test_support_visibility! {
const SCALA_GIVEN_FLAT_MARKER: &str = "given";
}
test_support_visibility! {
const SCALA2_DIALECT_MODIFIER: &str = "scala2";
}

pub fn classify_group_scala(module_path: &str) -> ImportGroup {
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

pub fn scala_block_uses_scala2_dialect(block: &ImportBlock) -> bool {
    block.imports.iter().any(|imp| {
        imp.raw_text.contains("=>")
            || imp.raw_text.contains("._")
            || imp.names.iter().any(|name| name.trim() == "_")
    })
}

pub fn parse_scala_imports(source: &str, tree: &Tree) -> ImportBlock {
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

pub fn generate_scala_import_line(req: &ImportRequest) -> String {
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
