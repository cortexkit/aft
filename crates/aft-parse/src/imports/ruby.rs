use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

const RUBY_REQUIRE_KIND: &str = "require";
const RUBY_REQUIRE_RELATIVE_KIND: &str = "require_relative";
/// Ruby's `load` re-runs the target every call, unlike the idempotent
/// `require` family — organize must never drop a repeat as a duplicate.
/// Exported so the organizer names this form rather than matching a literal.
pub const RUBY_LOAD_KIND: &str = "load";
const RUBY_QUOTE_SINGLE: &str = "quote:single";
const RUBY_QUOTE_DOUBLE: &str = "quote:double";

pub fn classify_group_ruby(_module_path: &str) -> ImportGroup {
    // Ruby has no conventional stdlib/external/internal declaration grouping for
    // require-like calls, so keep grouping neutral and stable.
    ImportGroup::External
}

pub fn parse_ruby_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "call" {
                if let Some(imp) = parse_ruby_require_call(source, &node) {
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

fn parse_ruby_require_call(source: &str, node: &Node) -> Option<ImportStatement> {
    let import_kind = ruby_top_level_require_method(source, node)?;
    let arg_list = find_direct_child(node, "argument_list")?;
    let string = find_direct_child(&arg_list, "string")?;
    let (module_path, quote) = ruby_string_literal_content(source, &string)?;
    if module_path.is_empty() {
        return None;
    }

    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();
    let group = classify_group_ruby(&module_path);
    let quote_modifier = ruby_quote_modifier(quote).to_string();
    let flat_marker = ruby_flat_marker(&import_kind, quote);

    Some(ImportStatement {
        module_path,
        names: Vec::new(),
        // The flat marker preserves Ruby's require form and quote style through
        // legacy organize code paths until all readers consume `form` directly.
        default_import: Some(flat_marker),
        namespace_import: None,
        kind: ImportKind::SideEffect,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: Vec::new(),
            namespace: None,
            alias: None,
            modifiers: vec![quote_modifier],
            import_kind: Some(import_kind),
        },
    })
}

fn ruby_top_level_require_method(source: &str, node: &Node) -> Option<String> {
    let mut method_name = None;
    let mut saw_receiver_separator = false;

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "." | "::" => saw_receiver_separator = true,
                "identifier" if method_name.is_none() => {
                    method_name = Some(source[child.byte_range()].trim().to_string());
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if saw_receiver_separator {
        return None;
    }

    let method_name = method_name?;
    if is_ruby_require_kind(&method_name) {
        Some(method_name)
    } else {
        None
    }
}

fn is_ruby_require_kind(kind: &str) -> bool {
    matches!(
        kind,
        RUBY_REQUIRE_KIND | RUBY_REQUIRE_RELATIVE_KIND | RUBY_LOAD_KIND
    )
}

fn ruby_string_literal_content(source: &str, node: &Node) -> Option<(String, char)> {
    let raw = source[node.byte_range()].trim();
    let quote = raw.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    if raw.chars().last()? != quote {
        return None;
    }

    // Interpolated requires are dynamic runtime expressions, not static import
    // declarations that AFT can safely sort/remove/regenerate.
    if raw.contains("#{") {
        return None;
    }

    let content = &raw[quote.len_utf8()..raw.len() - quote.len_utf8()];
    Some((content.to_string(), quote))
}

fn ruby_quote_modifier(quote: char) -> &'static str {
    if quote == '"' {
        RUBY_QUOTE_DOUBLE
    } else {
        RUBY_QUOTE_SINGLE
    }
}

fn ruby_flat_marker(import_kind: &str, quote: char) -> String {
    let quote_suffix = ruby_quote_suffix(quote);
    if quote_suffix.is_empty() {
        import_kind.to_string()
    } else {
        format!("{import_kind}|{quote_suffix}")
    }
}

fn ruby_quote_suffix(quote: char) -> &'static str {
    if quote == '"' {
        "double"
    } else {
        ""
    }
}

fn ruby_marker_parts(marker: &str) -> (Option<&str>, Option<char>) {
    let (kind, quote) = marker.split_once('|').unwrap_or((marker, ""));
    let kind = is_ruby_require_kind(kind).then_some(kind);
    let quote = match quote {
        "double" | RUBY_QUOTE_DOUBLE => Some('"'),
        "single" | RUBY_QUOTE_SINGLE => Some('\''),
        _ => None,
    };
    (kind, quote)
}

fn ruby_quote_from_modifiers(modifiers: &[String]) -> Option<char> {
    if modifiers
        .iter()
        .any(|modifier| modifier == RUBY_QUOTE_DOUBLE)
    {
        Some('"')
    } else if modifiers
        .iter()
        .any(|modifier| modifier == RUBY_QUOTE_SINGLE)
    {
        Some('\'')
    } else {
        None
    }
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

pub fn generate_ruby_import_line(req: &ImportRequest) -> String {
    let (marker_kind, marker_quote) = req
        .default_import
        .map(ruby_marker_parts)
        .unwrap_or((None, None));
    let import_kind = req
        .import_kind
        .filter(|kind| is_ruby_require_kind(kind))
        .or(marker_kind)
        .unwrap_or(RUBY_REQUIRE_KIND);
    let quote = ruby_quote_from_modifiers(req.modifiers)
        .or(marker_quote)
        .unwrap_or('\'');

    format!("{import_kind} {quote}{}{quote}", req.module_path)
}

pub struct RubySyntax;

impl ImportSyntax for RubySyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_ruby_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_ruby_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_ruby(module_path)
    }
}

pub static RUBY_SYNTAX: RubySyntax = RubySyntax;
