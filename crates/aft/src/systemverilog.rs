//! Written-source HDL declarations. No preprocessing or elaboration is performed.

use tree_sitter::Node;

use crate::error::AftError;
use crate::parser::{node_range_with_decorators, node_text, LangId, MAX_SIGNATURE_BYTES};
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
    let (kind, name) = match node.kind() {
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
        // The alias is a direct child; nested field/type names are not its name.
        "type_declaration" => (SymbolKind::TypeAlias, identifier(node)?),
        _ => return None,
    };
    Some((kind, name))
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
    let mut cursor = node.walk();
    for _ in 0..MAX_SIGNATURE_BYTES {
        let current = cursor.node();
        if current.start_byte() >= limit {
            break;
        }
        if current.kind() == ";" {
            end = current.end_byte();
            break;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
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
    let mut signature = source[node.start_byte()..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if end < node.end_byte() && !source[node.start_byte()..end].ends_with(';') {
        signature.push('…');
    }
    signature
}

pub(crate) fn extract_symbols(source: &str, root: Node<'_>) -> Result<Vec<Symbol>, AftError> {
    // A cursor keeps traversal memory independent of the number of siblings.
    // Bound copied names/scopes before retaining them, including malformed trees.
    const MAX_NODES: usize = 200_000;
    const MAX_SYMBOLS: usize = 20_000;
    const MAX_SCOPE_DEPTH: usize = 128;
    const MAX_COPIED_BYTES: usize = 8 * 1024 * 1024;
    let limit = || AftError::ParseError {
        message: "SystemVerilog symbol extraction limit exceeded".into(),
    };
    let mut cursor = root.walk();
    let mut scopes: Vec<(usize, String, bool)> = Vec::new();
    let mut symbols = Vec::new();
    let mut nodes = 0usize;
    let mut copied = 0usize;
    loop {
        nodes += 1;
        if nodes > MAX_NODES {
            return Err(limit());
        }
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
                range: node_range_with_decorators(&extent, source, LangId::SystemVerilog),
                signature: Some(signature(source, extent)),
                parent: scopes.last().map(|scope| scope.1.clone()),
                scope_chain: scopes.iter().map(|scope| scope.1.clone()).collect(),
                exported: false,
            });
            if node.kind() != "type_declaration" {
                scopes.push((
                    node.end_byte(),
                    name.into(),
                    node.kind() == "class_declaration",
                ));
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
}

/// Encode metavariables without modifying HDL system names or literal text.
pub(crate) fn preprocess_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    let mut previous = ' ';
    while let Some(c) = chars.next() {
        if c == '"' {
            out.push(c);
            while let Some(c) = chars.next() {
                out.push(c);
                if c == '\\' {
                    if let Some(c) = chars.next() {
                        out.push(c);
                    }
                } else if c == '"' {
                    break;
                }
            }
        } else if c == '\\' {
            out.push(c);
            for c in chars.by_ref() {
                out.push(c);
                if c.is_whitespace() {
                    break;
                }
            }
        } else if c == '/' && matches!(chars.peek(), Some('/' | '*')) {
            out.push(c);
            let style = chars.next().unwrap();
            out.push(style);
            let mut prev = style;
            for c in chars.by_ref() {
                out.push(c);
                if (style == '/' && c == '\n') || (style == '*' && prev == '*' && c == '/') {
                    break;
                }
                prev = c;
            }
        } else if c == '$'
            && !(previous.is_ascii_alphanumeric() || previous == '_' || previous == '$')
        {
            let mut count = 1;
            while chars.peek() == Some(&'$') {
                chars.next();
                count += 1;
            }
            if count <= 3
                && (count == 3
                    || chars
                        .peek()
                        .is_some_and(|c| c.is_ascii_uppercase() || *c == '_'))
            {
                out.push_str(&format!("__aft_meta_{count}_"));
            } else {
                out.extend(std::iter::repeat_n('$', count));
            }
        } else {
            out.push(c);
        }
        previous = c;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{detect_language, extract_symbols_from_tree, grammar_for};
    use std::path::Path;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar_for(LangId::SystemVerilog))
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn systemverilog_design_parses_packages_interfaces_and_includes() {
        for (name, source, expected) in [
            (
                "config.svh",
                include_str!("../tests/fixtures/systemverilog/config.svh"),
                None,
            ),
            (
                "types_pkg.sv",
                include_str!("../tests/fixtures/systemverilog/types_pkg.sv"),
                Some("types_pkg"),
            ),
            (
                "stream_if.sv",
                include_str!("../tests/fixtures/systemverilog/stream_if.sv"),
                Some("stream_if"),
            ),
            (
                "register_slice.sv",
                include_str!("../tests/fixtures/systemverilog/register_slice.sv"),
                Some("register_slice"),
            ),
            (
                "top.sv",
                include_str!("../tests/fixtures/systemverilog/top.sv"),
                Some("top"),
            ),
        ] {
            let tree = parse(source);
            assert!(
                !tree.root_node().has_error(),
                "{name}: {}",
                tree.root_node().to_sexp()
            );
            let symbols = extract_symbols_from_tree(source, &tree, LangId::SystemVerilog).unwrap();
            if let Some(expected) = expected {
                assert!(
                    symbols.iter().any(|symbol| symbol.name == expected),
                    "{name}: {symbols:?}"
                );
            }
        }
        for extension in ["sv", "svh", "v", "vh"] {
            assert_eq!(
                detect_language(Path::new(&format!("design.{extension}"))),
                Some(LangId::SystemVerilog)
            );
        }
        let verilog = "module legacy(a, b); input a; output b; assign b = a; endmodule";
        assert!(!parse(verilog).root_node().has_error());
    }

    #[test]
    fn systemverilog_symbols_keep_physical_scopes_ranges_and_signatures() {
        let source = "package first;\n  typedef logic [7:0] word_t;\n  function automatic int same(input int n);\n    return n + 1;\n  endfunction\n  task report(); endtask\nendpackage\npackage second;\n  function int same(); return 2; endfunction\nendpackage\nclass monitor;\n  function int same(); return 3; endfunction\nendclass\n";
        let tree = parse(source);
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let symbols = extract_symbols(source, tree.root_node()).unwrap();
        let same = symbols
            .iter()
            .filter(|s| s.name == "same")
            .collect::<Vec<_>>();
        assert_eq!(same.len(), 3, "{symbols:?}");
        assert_eq!(same[0].scope_chain, ["first"]);
        assert_eq!(same[1].scope_chain, ["second"]);
        assert_eq!(same[2].scope_chain, ["monitor"]);
        assert_eq!(same[2].kind, SymbolKind::Method);
        assert_eq!(same[0].parent.as_deref(), Some("first"));
        assert_eq!((same[0].range.start_line, same[0].range.end_line), (2, 4));
        assert_eq!(
            same[0].signature.as_deref(),
            Some("function automatic int same(input int n);")
        );
        assert!(symbols
            .iter()
            .any(|s| s.name == "word_t" && s.kind == SymbolKind::TypeAlias));
        assert!(symbols
            .iter()
            .any(|s| s.name == "report" && s.kind == SymbolKind::Task));
    }

    #[test]
    fn systemverilog_incomplete_source_retains_written_declarations() {
        let source = "module good; endmodule\nmodule unfinished; function int broken(\n";
        let tree = parse(source);
        assert!(tree.root_node().has_error());
        assert!(extract_symbols(source, tree.root_node())
            .unwrap()
            .iter()
            .any(|s| s.name == "good"));
    }

    #[test]
    fn systemverilog_headers_preserve_literal_semicolons_and_escaped_names() {
        let source = "module \\design.name #(parameter MSG = \"a;b\") ();\nendmodule\n";
        let tree = parse(source);
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let symbols = extract_symbols(source, tree.root_node()).unwrap();
        assert_eq!(symbols[0].name.trim_end(), "\\design.name");
        assert_eq!(
            symbols[0].signature.as_deref(),
            Some("module \\design.name #(parameter MSG = \"a;b\") ();")
        );
    }

    #[test]
    fn systemverilog_symbol_producer_reports_exhausted_bounds() {
        let source = "module m; endmodule\n".repeat(20_001);
        let tree = parse(&source);
        let error = extract_symbols(&source, tree.root_node()).unwrap_err();
        assert!(error.to_string().contains("limit exceeded"));
    }

    #[test]
    fn systemverilog_pattern_preprocessing_preserves_hdl_literals() {
        let pattern =
            "$display(\"$ARG\", $ARG); // $COMMENT\n/* $BLOCK */ \\escaped$NAME foo$BAR($$$ARGS)";
        assert_eq!(preprocess_pattern(pattern), "$display(\"$ARG\", __aft_meta_1_ARG); // $COMMENT\n/* $BLOCK */ \\escaped$NAME foo$BAR(__aft_meta_3_ARGS)");
    }
}
