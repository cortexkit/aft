use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

const C_INCLUDE_SYSTEM_KIND: &str = "system";
const C_INCLUDE_LOCAL_KIND: &str = "local";

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
pub(crate) fn normalize_include_module(module: &str) -> (String, Option<&'static str>) {
    let trimmed = module.trim();
    if let Some(inner) = trimmed.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        (inner.trim().to_string(), Some(C_INCLUDE_SYSTEM_KIND))
    } else if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        (inner.trim().to_string(), Some(C_INCLUDE_LOCAL_KIND))
    } else {
        (module.to_string(), None)
    }
}

pub(crate) fn parse_c_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    fn collect(source: &str, node: Node<'_>, imports: &mut Vec<ImportStatement>) {
        if node.kind() == "preproc_include" {
            if let Some(import) = parse_c_include(source, &node) {
                imports.push(import);
            }
        } else if node.parent().is_none()
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
            for child in node.named_children(&mut cursor) {
                collect(source, child, imports);
            }
        }
    }
    collect(source, root, &mut imports);

    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

/// An include guard is the default insertion context, not an arbitrary feature
/// conditional. Its matching define must precede newly added includes.
pub(crate) fn default_include_context<'a>(
    source: &str,
    tree: &'a Tree,
) -> (Node<'a>, Option<usize>) {
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
pub(crate) fn c_insertion_block(source: &str, tree: &Tree, block: &ImportBlock) -> ImportBlock {
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

pub(crate) fn classify_group_c_import_kind(import_kind: &str) -> ImportGroup {
    // C/C++ include delimiters carry search-path semantics: angle/system
    // includes are grouped as stdlib, while quote/local includes are grouped as
    // external project/third-party headers. They are deliberately distinct.
    if import_kind == C_INCLUDE_SYSTEM_KIND {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

pub(crate) fn classify_group_c(_module_path: &str) -> ImportGroup {
    // The registry-level classifier only receives a header path, not the
    // delimiter. Default to system because the legacy add path also defaults to
    // generating an angle include when no `import_kind` is supplied. Parsed
    // includes use `classify_group_c_import_kind` and preserve the real delimiter.
    ImportGroup::Stdlib
}

pub(crate) fn generate_c_import_line(req: &ImportRequest) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_c_like(source: &str, lang: LangId) -> (Tree, ImportBlock) {
        let grammar = grammar_for(lang).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, lang);
        (tree, block)
    }

    fn structured(import: &ImportStatement) -> Option<&str> {
        match &import.form {
            ImportForm::Structured {
                named,
                namespace,
                alias,
                modifiers,
                import_kind,
            } => {
                assert!(named.is_empty());
                assert!(namespace.is_none());
                assert!(alias.is_none());
                assert!(modifiers.is_empty());
                import_kind.as_deref()
            }
            other => panic!("expected C/C++ Structured import, got {other:?}"),
        }
    }

    /// Grammar fixture: lock the exact tree-sitter-c and tree-sitter-cpp node
    /// kinds this parser depends on. Both grammars currently emit
    /// `preproc_include`, with `system_lib_string` for `<...>` headers and
    /// `string_literal` / `string_content` for `"..."` headers.
    #[test]
    fn c_include_grammar_node_kinds_are_stable() {
        for (lang, source) in [
            (
                LangId::C,
                "#include <stdio.h>\n#include \"local.h\"\nint main(void) { return 0; }\n",
            ),
            (
                LangId::Cpp,
                "#include <vector>\n#include \"foo.hpp\"\nint main() { return 0; }\n",
            ),
        ] {
            let (tree, _) = parse_c_like(source, lang);
            assert!(!tree.root_node().has_error(), "parse errors for {lang:?}");

            let mut kinds = BTreeSet::new();
            fn walk(node: Node, kinds: &mut BTreeSet<String>) {
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
                "translation_unit",
                "preproc_include",
                "#include",
                "system_lib_string",
                "string_literal",
                "string_content",
            ] {
                assert!(
                    kinds.contains(required),
                    "{lang:?} grammar missing node kind {required:?}; present: {kinds:?}"
                );
            }
        }
    }

    #[test]
    fn parse_c_and_cpp_includes_preserves_delimiters() {
        for (lang, source, system_path, local_path) in [
            (
                LangId::C,
                "#include <stdio.h>\n#include \"project/header.h\"\nint main(void) { return 0; }\n",
                "stdio.h",
                "project/header.h",
            ),
            (
                LangId::Cpp,
                "#include <vector>\n#include \"foo/bar.hpp\"\nint main() { return 0; }\n",
                "vector",
                "foo/bar.hpp",
            ),
        ] {
            let (_, block) = parse_c_like(source, lang);
            assert_eq!(block.imports.len(), 2, "parse imports for {lang:?}");
            assert_c_include(
                &block.imports[0],
                system_path,
                C_INCLUDE_SYSTEM_KIND,
                ImportGroup::Stdlib,
            );
            assert_c_include(
                &block.imports[1],
                local_path,
                C_INCLUDE_LOCAL_KIND,
                ImportGroup::External,
            );
        }
    }

    fn assert_c_include(
        imp: &ImportStatement,
        module_path: &str,
        import_kind: &str,
        group: ImportGroup,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.default_import.as_deref(), Some(import_kind));
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::SideEffect);
        assert_eq!(imp.group, group);
        assert_eq!(structured(imp), Some(import_kind));
    }

    #[test]
    fn generate_c_and_cpp_supported_forms() {
        for lang in [LangId::C, LangId::Cpp] {
            assert_eq!(
                generate_import(
                    lang,
                    &ImportRequest {
                        module_path: "stdio.h",
                        names: &[],
                        default_import: None,
                        namespace: None,
                        alias: None,
                        type_only: false,
                        modifiers: &[],
                        import_kind: Some(C_INCLUDE_SYSTEM_KIND),
                    },
                ),
                "#include <stdio.h>"
            );
            assert_eq!(
                generate_import(
                    lang,
                    &ImportRequest {
                        module_path: "project/header.h",
                        names: &[],
                        default_import: None,
                        namespace: None,
                        alias: None,
                        type_only: false,
                        modifiers: &[],
                        import_kind: Some(C_INCLUDE_LOCAL_KIND),
                    },
                ),
                "#include \"project/header.h\""
            );
        }
    }

    #[test]
    fn generate_c_preserves_organized_flat_markers() {
        for (marker, expected) in [
            (C_INCLUDE_SYSTEM_KIND, "#include <stdio.h>"),
            (C_INCLUDE_LOCAL_KIND, "#include \"stdio.h\""),
        ] {
            assert_eq!(
                generate_import(
                    LangId::C,
                    &ImportRequest::legacy("stdio.h", &[], Some(marker), None, false),
                ),
                expected
            );
        }
    }

    #[test]
    fn c_and_cpp_round_trip_through_parse_generate() {
        for (lang, samples) in [
            (
                LangId::C,
                ["#include <stdio.h>", "#include \"project/header.h\""],
            ),
            (LangId::Cpp, ["#include <vector>", "#include \"foo.hpp\""]),
        ] {
            for src in samples {
                let (_, block) = parse_c_like(src, lang);
                assert_eq!(block.imports.len(), 1, "parse {src:?} for {lang:?}");
                let imp = &block.imports[0];
                let import_kind = structured(imp);
                let regenerated = generate_import(
                    lang,
                    &ImportRequest {
                        module_path: &imp.module_path,
                        names: &imp.names,
                        default_import: imp.default_import.as_deref(),
                        namespace: None,
                        alias: None,
                        type_only: false,
                        modifiers: &[],
                        import_kind,
                    },
                );
                assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
            }
        }
    }

    #[test]
    fn normalize_include_module_strips_delimiters_and_infers_kind() {
        // Angle/system delimiter.
        assert_eq!(
            normalize_include_module("<vector>"),
            ("vector".to_string(), Some(C_INCLUDE_SYSTEM_KIND))
        );
        // Quote/local delimiter.
        assert_eq!(
            normalize_include_module("\"foo/bar.h\""),
            ("foo/bar.h".to_string(), Some(C_INCLUDE_LOCAL_KIND))
        );
        // Surrounding whitespace tolerated.
        assert_eq!(
            normalize_include_module("  <stdio.h>  "),
            ("stdio.h".to_string(), Some(C_INCLUDE_SYSTEM_KIND))
        );
        // Bare header: unchanged, no inferred kind (caller defaults to system).
        assert_eq!(
            normalize_include_module("string"),
            ("string".to_string(), None)
        );
    }
}
