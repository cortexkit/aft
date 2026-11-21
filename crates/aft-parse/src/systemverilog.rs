//! Written-source HDL declarations. No preprocessing or elaboration is performed.

use tree_sitter::Node;

use crate::error::AftError;
use crate::parser::{node_range, node_text, MAX_SIGNATURE_BYTES};
use crate::symbols::{Symbol, SymbolKind};

fn child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|node| node.kind() == kind);
    found
}

fn identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).find(|node| {
        matches!(node.kind(), "simple_identifier" | "escaped_identifier")
            || node.kind().ends_with("_identifier")
    });
    found
}

fn header_name<'a>(node: Node<'a>, headers: &[&str]) -> Option<Node<'a>> {
    headers
        .iter()
        .find_map(|kind| child(node, kind).and_then(identifier))
        .or_else(|| identifier(node))
}

fn declaration(node: Node<'_>) -> Option<(SymbolKind, Node<'_>)> {
    Some(match node.kind() {
        "module_declaration" => (
            SymbolKind::Module,
            header_name(node, &["module_ansi_header", "module_nonansi_header"])?,
        ),
        "interface_declaration" => (
            SymbolKind::Interface,
            header_name(node, &["interface_ansi_header", "interface_nonansi_header"])?,
        ),
        "package_declaration" => (SymbolKind::Package, identifier(node)?),
        "class_declaration" => (SymbolKind::Class, identifier(node)?),
        "interface_class_declaration" => (SymbolKind::Interface, identifier(node)?),
        "function_declaration" => (
            SymbolKind::Function,
            identifier(child(node, "function_body_declaration")?)?,
        ),
        "task_declaration" => (
            SymbolKind::Task,
            identifier(child(node, "task_body_declaration")?)?,
        ),
        "function_prototype" => (SymbolKind::Function, identifier(node)?),
        "task_prototype" => (SymbolKind::Task, identifier(node)?),
        // In tree-sitter-systemverilog 0.4.1 the alias is the first direct identifier;
        // nested field/type names are not the alias. See the outline fixture test.
        "type_declaration" => (SymbolKind::TypeAlias, identifier(node)?),
        _ => return None,
    })
}

fn extent(node: Node<'_>) -> Node<'_> {
    match node.parent() {
        Some(parent)
            if matches!(
                parent.kind(),
                "class_method" | "interface_class_method" | "dpi_import_export"
            ) =>
        {
            parent
        }
        _ => node,
    }
}

fn signature(source: &str, node: Node<'_>) -> String {
    // Find a syntax token, not a semicolon inside a parameter string/comment.
    let limit = node
        .end_byte()
        .min(node.start_byte().saturating_add(MAX_SIGNATURE_BYTES));
    let mut end = limit;
    let mut brace_depth = 0usize;
    let mut cursor = node.walk();
    for _ in 0..MAX_SIGNATURE_BYTES {
        let current = cursor.node();
        if current.start_byte() >= limit {
            break;
        }
        match current.kind() {
            "{" => brace_depth += 1,
            "}" => brace_depth = brace_depth.saturating_sub(1),
            ";" if brace_depth == 0 => {
                end = current.end_byte();
                break;
            }
            _ => {}
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() || !cursor.goto_parent() {
                break;
            }
        }
        if cursor.node() == node {
            break;
        }
    }
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let snippet = &source[node.start_byte()..end];
    let mut signature = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    if end < node.end_byte() && !snippet.ends_with(';') {
        signature.push('…');
    }
    signature
}

pub(crate) fn extract_symbols(source: &str, root: Node<'_>) -> Result<Vec<Symbol>, AftError> {
    // Bound the traversal and copies, including on malformed trees.
    const MAX_NODES: usize = 200_000;
    const MAX_SYMBOLS: usize = 20_000;
    const MAX_SCOPE_DEPTH: usize = 128;
    const MAX_COPIED_BYTES: usize = 8 * 1024 * 1024;
    let limit = || AftError::ParseError {
        message: "SystemVerilog symbol extraction limit exceeded".into(),
    };
    let mut cursor = root.walk();
    let mut scopes: Vec<(usize, &str, bool)> = Vec::new();
    let mut symbols = Vec::new();
    let mut copied = 0usize;
    for _ in 0..MAX_NODES {
        let node = cursor.node();
        while scopes
            .last()
            .is_some_and(|scope| scope.0 <= node.start_byte())
        {
            scopes.pop();
        }
        if let Some((mut kind, name_node)) = declaration(node) {
            let name = node_text(source, &name_node);
            let size = name
                .len()
                .saturating_mul(2)
                .saturating_add(MAX_SIGNATURE_BYTES)
                .saturating_add(scopes.iter().map(|scope| scope.1.len()).sum::<usize>() * 2);
            copied = copied.saturating_add(size);
            if symbols.len() >= MAX_SYMBOLS
                || scopes.len() >= MAX_SCOPE_DEPTH
                || copied > MAX_COPIED_BYTES
            {
                return Err(limit());
            }
            if kind == SymbolKind::Function && scopes.iter().any(|scope| scope.2) {
                kind = SymbolKind::Method;
            }
            let extent = extent(node);
            symbols.push(Symbol {
                name: name.into(),
                kind,
                range: node_range(&extent),
                signature: Some(signature(source, extent)),
                parent: scopes.last().map(|scope| scope.1.into()),
                scope_chain: scopes.iter().map(|scope| scope.1.into()).collect(),
                exported: false,
            });
            if node.kind() != "type_declaration" {
                scopes.push((node.end_byte(), name, node.kind() == "class_declaration"));
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(symbols);
            }
        }
    }
    Err(limit())
}
