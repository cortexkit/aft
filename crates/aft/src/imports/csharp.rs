use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

/// Classify a C# using path: `System`, `System.*`, `Microsoft`, and
/// `Microsoft.*` are treated as standard-library/framework imports; everything
/// else is external. C# has no relative/internal import syntax.
pub(crate) fn classify_group_csharp(module_path: &str) -> ImportGroup {
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
pub(crate) fn parse_csharp_imports(source: &str, tree: &Tree) -> ImportBlock {
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
pub(crate) fn generate_csharp_import_line(req: &ImportRequest) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_csharp(source: &str) -> ImportBlock {
        let grammar = grammar_for(LangId::CSharp).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        parse_imports(source, &tree, LangId::CSharp)
    }

    /// Grammar fixture: lock the tree-sitter-c-sharp node kinds the parser
    /// depends on. The current grammar emits `using_directive` with direct
    /// keyword/token children (`global`, `using`, `static`, `unsafe`, `=`, `;`)
    /// and path nodes (`identifier` / `qualified_name`).
    #[test]
    fn csharp_grammar_node_kinds_are_stable() {
        let grammar = grammar_for(LangId::CSharp).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let src = "using System;\nusing static System.Math;\nusing unsafe System.Buffer;\nusing Con = System.Console;\nusing unsafe Ptr = System.IntPtr;\nglobal using System;\nglobal using static unsafe System.Math;\nnamespace App;\nclass C {}\n";
        let tree = parser.parse(src, None).unwrap();
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
            "using_directive",
            "using",
            "identifier",
            "qualified_name",
            "static",
            "global",
            "unsafe",
            "=",
            ";",
        ] {
            assert!(
                kinds.contains(required),
                "C# grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_csharp_all_five_forms() {
        let block = parse_csharp(
            "using System;\nusing static System.Math;\nusing Con = System.Console;\nglobal using System;\nglobal using static System.Math;\nnamespace App;\nclass C {}\n",
        );
        assert_eq!(block.imports.len(), 5);

        assert_csharp_import(&block.imports[0], "System", &[], None);
        assert_csharp_import(&block.imports[1], "System.Math", &["static"], None);
        assert_csharp_import(&block.imports[2], "System.Console", &[], Some("Con"));
        assert_csharp_import(&block.imports[3], "System", &["global"], None);
        assert_csharp_import(
            &block.imports[4],
            "System.Math",
            &["global", "static"],
            None,
        );
    }

    #[test]
    fn parse_csharp_unsafe_using_forms() {
        let block = parse_csharp(
            "using unsafe System.Buffer;\nusing static unsafe System.Math;\nusing unsafe Ptr = System.IntPtr;\nclass C {}\n",
        );
        assert_eq!(block.imports.len(), 3);

        assert_csharp_import(&block.imports[0], "System.Buffer", &["unsafe"], None);
        assert_csharp_import(
            &block.imports[1],
            "System.Math",
            &["static", "unsafe"],
            None,
        );
        assert_csharp_import(&block.imports[2], "System.IntPtr", &["unsafe"], Some("Ptr"));
    }

    fn assert_csharp_import(
        imp: &ImportStatement,
        module_path: &str,
        modifiers: &[&str],
        alias: Option<&str>,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert!(imp.names.is_empty());
        assert_eq!(imp.default_import, None);
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, classify_group_csharp(module_path));

        match &imp.form {
            ImportForm::Structured {
                named,
                namespace,
                alias: form_alias,
                modifiers: form_modifiers,
                import_kind,
            } => {
                assert!(named.is_empty());
                assert_eq!(namespace, &None);
                assert_eq!(form_alias.as_deref(), alias);
                assert_eq!(
                    form_modifiers,
                    &modifiers
                        .iter()
                        .map(|modifier| modifier.to_string())
                        .collect::<Vec<_>>()
                );
                assert_eq!(import_kind, &None);
            }
            other => panic!("expected C# Structured import, got {other:?}"),
        }
    }

    #[test]
    fn generate_csharp_all_forms() {
        assert_eq!(
            generate_import(
                LangId::CSharp,
                &ImportRequest::legacy("System", &[], None, None, false)
            ),
            "using System;"
        );

        let static_modifiers = vec!["static".to_string()];
        assert_eq!(
            generate_import(
                LangId::CSharp,
                &ImportRequest {
                    module_path: "System.Math",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &static_modifiers,
                    import_kind: None,
                }
            ),
            "using static System.Math;"
        );

        assert_eq!(
            generate_import(
                LangId::CSharp,
                &ImportRequest {
                    module_path: "System.Console",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("Con"),
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                }
            ),
            "using Con = System.Console;"
        );

        let global_modifiers = vec!["global".to_string()];
        assert_eq!(
            generate_import(
                LangId::CSharp,
                &ImportRequest {
                    module_path: "System",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &global_modifiers,
                    import_kind: None,
                }
            ),
            "global using System;"
        );

        let global_static_modifiers = vec!["global".to_string(), "static".to_string()];
        assert_eq!(
            generate_import(
                LangId::CSharp,
                &ImportRequest {
                    module_path: "System.Math",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &global_static_modifiers,
                    import_kind: None,
                }
            ),
            "global using static System.Math;"
        );

        let unsafe_modifiers = vec!["unsafe".to_string()];
        assert_eq!(
            generate_import(
                LangId::CSharp,
                &ImportRequest {
                    module_path: "System.Buffer",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &unsafe_modifiers,
                    import_kind: None,
                }
            ),
            "using unsafe System.Buffer;"
        );
        assert_eq!(
            generate_import(
                LangId::CSharp,
                &ImportRequest {
                    module_path: "System.IntPtr",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("Ptr"),
                    type_only: false,
                    modifiers: &unsafe_modifiers,
                    import_kind: None,
                }
            ),
            "using unsafe Ptr = System.IntPtr;"
        );
    }

    #[test]
    fn classify_group_csharp_framework_vs_external() {
        assert_eq!(classify_group_csharp("System"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_csharp("System.Net.Http"),
            ImportGroup::Stdlib
        );
        assert_eq!(
            classify_group_csharp("Microsoft.Extensions.Logging"),
            ImportGroup::Stdlib
        );
        assert_eq!(
            classify_group_csharp("Newtonsoft.Json"),
            ImportGroup::External
        );
        assert_eq!(classify_group_csharp("App.Core"), ImportGroup::External);
    }

    #[test]
    fn csharp_round_trips_through_parse_generate() {
        for src in [
            "using System;",
            "using static System.Math;",
            "using Con = System.Console;",
            "global using System;",
            "global using static System.Math;",
            "using unsafe System.Buffer;",
            "using static unsafe System.Math;",
            "using unsafe Ptr = System.IntPtr;",
        ] {
            let block = parse_csharp(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (alias, modifiers) = match &imp.form {
                ImportForm::Structured {
                    alias, modifiers, ..
                } => (alias.as_deref(), modifiers.as_slice()),
                other => panic!("expected C# Structured import, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::CSharp,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias,
                    type_only: false,
                    modifiers,
                    import_kind: None,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
