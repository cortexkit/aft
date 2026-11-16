use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

/// Classify a C# using path: `System`, `System.*`, `Microsoft`, and
/// `Microsoft.*` are treated as standard-library/framework imports; everything
/// else is external. C# has no relative/internal import syntax.
pub fn classify_group_csharp(module_path: &str) -> ImportGroup {
    if module_path == "System"
        || module_path.starts_with("System.")
        || module_path == "Microsoft"
        || module_path.starts_with("Microsoft.")
    {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

/// Parse C# `using_directive` nodes into the generic structured import form.
pub fn parse_csharp_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    collect_csharp_imports(source, root, &mut imports);
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

/// Collects the using directives under `root` in source order, without
/// recursion, so a deeply nested file cannot overflow the stack.
fn collect_csharp_imports(source: &str, root: Node, imports: &mut Vec<ImportStatement>) {
    super::for_each_preorder_until(root, "using_directive", |node| {
        if let Some(imp) = parse_csharp_using_directive(source, &node) {
            imports.push(imp);
        }
    });
}

fn parse_csharp_using_directive(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    let mut children: Vec<(String, String)> = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            children.push((
                child.kind().to_string(),
                source[child.byte_range()].to_string(),
            ));
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let mut modifiers = Vec::new();
    if children.iter().any(|(kind, _)| kind == "global") {
        modifiers.push("global".to_string());
    }
    if children.iter().any(|(kind, _)| kind == "static") {
        modifiers.push("static".to_string());
    }
    if children.iter().any(|(kind, _)| kind == "unsafe") {
        modifiers.push("unsafe".to_string());
    }

    let equals_pos = children.iter().position(|(kind, _)| kind == "=");
    let alias = equals_pos.and_then(|idx| csharp_alias_before_equals(&children, idx));
    let module_path = if let Some(idx) = equals_pos {
        csharp_payload_text(&children[idx + 1..])?
    } else {
        let path_start = children
            .iter()
            .rposition(|(kind, _)| kind == "using" || kind == "static")
            .map(|idx| idx + 1)
            .unwrap_or(0);
        csharp_payload_text(&children[path_start..])?
    };

    if module_path.is_empty() {
        return None;
    }

    let group = classify_group_csharp(&module_path);

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
            modifiers,
            import_kind: None,
        },
    })
}

fn csharp_alias_before_equals(children: &[(String, String)], equals_pos: usize) -> Option<String> {
    children[..equals_pos]
        .iter()
        .rev()
        .find(|(kind, _)| kind == "identifier")
        .map(|(_, text)| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn csharp_payload_text(children: &[(String, String)]) -> Option<String> {
    children
        .iter()
        .find(|(kind, _)| !is_csharp_using_syntax_token(kind))
        .map(|(_, text)| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn is_csharp_using_syntax_token(kind: &str) -> bool {
    matches!(kind, "global" | "using" | "static" | "unsafe" | "=" | ";")
}

/// Generate a C# using directive from the generic structured import request.
pub fn generate_csharp_import_line(req: &ImportRequest) -> String {
    let has_global = req.modifiers.iter().any(|m| m == "global");
    let has_static = req.alias.is_none() && req.modifiers.iter().any(|m| m == "static");
    let has_unsafe = req.modifiers.iter().any(|m| m == "unsafe");

    let mut line = String::new();
    if has_global {
        line.push_str("global ");
    }
    line.push_str("using ");
    if has_static {
        line.push_str("static ");
    }
    if has_unsafe {
        line.push_str("unsafe ");
    }
    if let Some(alias) = req.alias {
        line.push_str(alias);
        line.push_str(" = ");
    }
    line.push_str(req.module_path);
    line.push(';');
    line
}

pub struct CSharpSyntax;
impl ImportSyntax for CSharpSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_csharp_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_csharp_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_csharp(module_path)
    }
}

pub static CSHARP_SYNTAX: CSharpSyntax = CSharpSyntax;
