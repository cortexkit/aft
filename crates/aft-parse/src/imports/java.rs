use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

pub fn classify_group_java(module_path: &str) -> ImportGroup {
    if module_path.starts_with("java.") || module_path.starts_with("javax.") {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

pub fn parse_java_imports(source: &str, tree: &Tree) -> ImportBlock {
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

pub fn generate_java_import_line(req: &ImportRequest) -> String {
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
