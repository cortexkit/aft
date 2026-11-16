use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

test_support_visibility! {
const C_INCLUDE_SYSTEM_KIND: &str = "system";
}
test_support_visibility! {
const C_INCLUDE_LOCAL_KIND: &str = "local";
}

/// Normalize an agent-supplied C/C++ include module.
///
/// Agents naturally pass the include with its delimiter (`<vector>` or
/// `"foo.h"`) because that is how includes are written. Strip the delimiter and
/// infer the include kind so dedup, classification, and generation all see the
/// bare header path — otherwise generation double-wraps into `#include
/// <<vector>>`, which fails syntax validation and rolls the edit back.
///
/// Returns the bare header path plus the inferred include kind, or the input
/// unchanged with `None` when no delimiter is present (the caller then honors an
/// explicit `import_kind`, defaulting to a system include).
pub fn normalize_include_module(module: &str) -> (String, Option<&'static str>) {
    let trimmed = module.trim();
    if let Some(inner) = trimmed.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        (inner.trim().to_string(), Some(C_INCLUDE_SYSTEM_KIND))
    } else if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        (inner.trim().to_string(), Some(C_INCLUDE_LOCAL_KIND))
    } else {
        (module.to_string(), None)
    }
}

pub fn parse_c_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    // An explicit work stack instead of recursion: conditional blocks can
    // nest arbitrarily deep, and a stack overflow aborts the process.
    // Children are pushed in reverse so includes come out in source order.
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "preproc_include" {
            if let Some(import) = parse_c_include(source, &node) {
                imports.push(import);
            }
        } else if node.id() == root.id()
            || matches!(
                node.kind(),
                "preproc_if"
                    | "preproc_ifdef"
                    | "preproc_elif"
                    | "preproc_else"
                    | "preproc_elifdef"
            )
        {
            let mut cursor = node.walk();
            let start = pending.len();
            pending.extend(node.named_children(&mut cursor));
            pending[start..].reverse();
        }
    }

    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

/// An include guard is the default insertion context, not an arbitrary feature
/// conditional. Its matching define must precede newly added includes.
pub fn default_include_context<'a>(source: &str, tree: &'a Tree) -> (Node<'a>, Option<usize>) {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let first = root
        .named_children(&mut cursor)
        .find(|child| child.kind() != "comment");
    if let Some(guard) = first.filter(|node| node.kind() == "preproc_ifdef") {
        let text = crate::parser::node_text(source, &guard);
        if let Some(macro_name) = text
            .lines()
            .next()
            .and_then(|line| line.trim().strip_prefix('#'))
            .and_then(|line| line.trim_start().strip_prefix("ifndef"))
            .and_then(|line| line.split_whitespace().next())
        {
            let mut cursor = guard.walk();
            for child in guard.named_children(&mut cursor) {
                if child.kind() == "preproc_def" {
                    let directive = crate::parser::node_text(source, &child)
                        .trim_start()
                        .strip_prefix('#')
                        .unwrap_or("");
                    let mut words = directive.split_whitespace();
                    if words.next() == Some("define") && words.next() == Some(macro_name) {
                        return (guard, Some(child.end_byte()));
                    }
                }
            }
        }
    }
    (root, None)
}

/// Keep feature-conditional includes visible to readers, but do not use them
/// as duplicate or ordering anchors for a default-context addition.
pub fn c_insertion_block(source: &str, tree: &Tree, block: &ImportBlock) -> ImportBlock {
    let (context, _) = default_include_context(source, tree);
    let imports = block
        .imports
        .iter()
        .filter(|import| {
            context
                .named_descendant_for_byte_range(import.byte_range.start, import.byte_range.end)
                .and_then(|mut node| {
                    while node.kind() != "preproc_include" {
                        node = node.parent()?;
                    }
                    node.parent()
                })
                .is_some_and(|parent| parent.id() == context.id())
        })
        .cloned()
        .collect::<Vec<_>>();
    ImportBlock {
        byte_range: import_byte_range(&imports),
        imports,
    }
}

fn parse_c_include(source: &str, node: &Node) -> Option<ImportStatement> {
    let (module_path, import_kind) = c_include_target(source, node)?;
    if module_path.is_empty() {
        return None;
    }

    let byte_range = include_byte_range(source, node);
    let raw_text = source[byte_range.clone()].to_string();
    let group = classify_group_c_import_kind(import_kind);
    let import_kind_string = import_kind.to_string();

    Some(ImportStatement {
        module_path,
        names: Vec::new(),
        // Preserve the include delimiter through legacy organize paths, which
        // regenerate from flat fields before they reach the structured form.
        default_import: Some(import_kind_string.clone()),
        namespace_import: None,
        kind: ImportKind::SideEffect,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: Vec::new(),
            namespace: None,
            alias: None,
            modifiers: Vec::new(),
            import_kind: Some(import_kind_string),
        },
    })
}

fn include_byte_range(source: &str, node: &Node) -> std::ops::Range<usize> {
    let start = node.byte_range().start;
    let mut end = node.byte_range().end;
    let bytes = source.as_bytes();
    while end > start && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    start..end
}

fn c_include_target(source: &str, node: &Node) -> Option<(String, &'static str)> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "system_lib_string" => {
                    let module_path = delimited_include_path(source, &child, '<', '>')?;
                    return Some((module_path, C_INCLUDE_SYSTEM_KIND));
                }
                "string_literal" => {
                    let module_path = delimited_include_path(source, &child, '"', '"')?;
                    return Some((module_path, C_INCLUDE_LOCAL_KIND));
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn delimited_include_path(source: &str, node: &Node, open: char, close: char) -> Option<String> {
    let raw = source[node.byte_range()].trim();
    if !raw.starts_with(open) || !raw.ends_with(close) {
        return None;
    }

    let start = open.len_utf8();
    let end = raw.len().checked_sub(close.len_utf8())?;
    let module_path = raw[start..end].trim();
    if module_path.is_empty() {
        None
    } else {
        Some(module_path.to_string())
    }
}

pub fn classify_group_c_import_kind(import_kind: &str) -> ImportGroup {
    // C/C++ include delimiters carry search-path semantics: angle/system
    // includes are grouped as stdlib, while quote/local includes are grouped as
    // external project/third-party headers. They are deliberately distinct.
    if import_kind == C_INCLUDE_SYSTEM_KIND {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

pub fn classify_group_c(_module_path: &str) -> ImportGroup {
    // The registry-level classifier only receives a header path, not the
    // delimiter. Default to system because the legacy add path also defaults to
    // generating an angle include when no `import_kind` is supplied. Parsed
    // includes use `classify_group_c_import_kind` and preserve the real delimiter.
    ImportGroup::Stdlib
}

pub fn generate_c_import_line(req: &ImportRequest) -> String {
    let import_kind = req
        .import_kind
        .or(req.default_import)
        .unwrap_or(C_INCLUDE_SYSTEM_KIND);

    if import_kind == C_INCLUDE_LOCAL_KIND {
        format!("#include \"{}\"", req.module_path)
    } else {
        format!("#include <{}>", req.module_path)
    }
}

pub struct CSyntax;

impl ImportSyntax for CSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_c_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_c_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_c(module_path)
    }
}

pub static C_SYNTAX: CSyntax = CSyntax;
