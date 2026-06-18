use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

const PERL_USE_KIND: &str = "use";
const PERL_REQUIRE_KIND: &str = "require";
const PERL_NO_KIND: &str = "no";
const PERL_FLAT_MARKER_PREFIX: &str = "perl:";

pub(crate) fn classify_group_perl(_module_path: &str) -> ImportGroup {
    // Perl pragmas/modules do not have a source-level stdlib/external/internal
    // grouping convention, so keep Phase 1 grouping neutral and stable.
    ImportGroup::External
}

pub(crate) fn parse_perl_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if let Some(imp) = parse_perl_import_statement(source, &node) {
                imports.push(imp);
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

fn parse_perl_import_statement(source: &str, node: &Node) -> Option<ImportStatement> {
    match node.kind() {
        // `use`/`no` pragmas — including `use parent ...` and `use constant ...`
        // — all parse as a single `use_statement`. The leading `use`/`no`
        // keyword carries the kind and the `module` field carries the pragma
        // name ("parent", "constant", or an ordinary module), so no per-pragma
        // special-casing is needed.
        "use_statement" => parse_perl_use_statement(source, node),
        // `require Foo;` parses as an expression statement wrapping a
        // `require_expression`.
        "expression_statement" => {
            let require = find_direct_child(node, "require_expression")?;
            let module_node = first_named_child(&require)?;
            build_perl_import(source, node, &module_node, PERL_REQUIRE_KIND)
        }
        _ => None,
    }
}

fn parse_perl_use_statement(source: &str, node: &Node) -> Option<ImportStatement> {
    let import_kind = if find_direct_child(node, PERL_USE_KIND).is_some() {
        PERL_USE_KIND
    } else if find_direct_child(node, PERL_NO_KIND).is_some() {
        PERL_NO_KIND
    } else {
        return None;
    };
    let module_node = node.child_by_field_name("module")?;
    build_perl_import(source, node, &module_node, import_kind)
}

fn build_perl_import(
    source: &str,
    statement: &Node,
    module_node: &Node,
    import_kind: &str,
) -> Option<ImportStatement> {
    let module_path = source[module_node.byte_range()].trim().to_string();
    if module_path.is_empty() {
        return None;
    }

    let raw_args = raw_args_after_module(source, statement, module_node)?;
    let modifiers = perl_arg_modifiers(&raw_args);
    // `use_statement` spans its own terminating `;`, but an `expression_statement`
    // (e.g. `require Foo;`) does not — the `;` is a following sibling. Extend the
    // range through it so organize/remove replaces the whole statement and leaves
    // no stray semicolon behind.
    let end_byte = match statement.next_sibling() {
        Some(next) if next.kind() == ";" => next.end_byte(),
        _ => statement.end_byte(),
    };
    let raw_text = source[statement.start_byte()..end_byte].to_string();
    let byte_range = statement.start_byte()..end_byte;
    let group = classify_group_perl(&module_path);

    Some(ImportStatement {
        module_path,
        names: Vec::new(),
        // Generic organize only carries flat fields into the generator. Preserve
        // Perl's statement kind and raw argument tail here until it consumes
        // `ImportForm::Structured` directly.
        default_import: Some(perl_flat_marker(import_kind, &raw_args)),
        namespace_import: None,
        kind: ImportKind::SideEffect,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: Vec::new(),
            namespace: None,
            alias: None,
            modifiers,
            import_kind: Some(import_kind.to_string()),
        },
    })
}

fn raw_args_after_module(source: &str, statement: &Node, module_node: &Node) -> Option<String> {
    let statement_end = find_direct_child(statement, ";")
        .map(|semicolon| semicolon.start_byte())
        .unwrap_or_else(|| statement.end_byte());
    if module_node.end_byte() > statement_end {
        return None;
    }

    Some(
        source[module_node.end_byte()..statement_end]
            .trim()
            .to_string(),
    )
}

fn perl_arg_modifiers(raw_args: &str) -> Vec<String> {
    if raw_args.is_empty() {
        Vec::new()
    } else {
        vec![raw_args.to_string()]
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

fn first_named_child<'tree>(node: &Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.is_named() {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn is_perl_import_kind(kind: &str) -> bool {
    matches!(kind, PERL_USE_KIND | PERL_REQUIRE_KIND | PERL_NO_KIND)
}

fn perl_flat_marker(import_kind: &str, raw_args: &str) -> String {
    format!(
        "{PERL_FLAT_MARKER_PREFIX}{}:{import_kind}{raw_args}",
        import_kind.len()
    )
}

fn perl_marker_parts(marker: &str) -> Option<(&str, &str)> {
    let payload = marker.strip_prefix(PERL_FLAT_MARKER_PREFIX)?;
    let (kind_len, rest) = payload.split_once(':')?;
    let kind_len = kind_len.parse::<usize>().ok()?;
    if rest.len() < kind_len || !rest.is_char_boundary(kind_len) {
        return None;
    }

    let (kind, raw_args) = rest.split_at(kind_len);
    is_perl_import_kind(kind).then_some((kind, raw_args))
}

fn perl_args_from_modifiers(modifiers: &[String]) -> Option<String> {
    let raw_args = modifiers
        .iter()
        .map(|modifier| modifier.trim())
        .filter(|modifier| !modifier.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!raw_args.is_empty()).then_some(raw_args)
}

pub(crate) fn generate_perl_import_line(req: &ImportRequest) -> String {
    let (marker_kind, marker_args) = req
        .default_import
        .and_then(perl_marker_parts)
        .map(|(kind, raw_args)| (Some(kind), Some(raw_args)))
        .unwrap_or((None, None));
    let import_kind = req
        .import_kind
        .filter(|kind| is_perl_import_kind(kind))
        .or(marker_kind)
        .unwrap_or(PERL_USE_KIND);
    let raw_args = perl_args_from_modifiers(req.modifiers)
        .or_else(|| marker_args.map(str::to_string))
        .unwrap_or_default();

    let mut line = String::new();
    line.push_str(import_kind);
    line.push(' ');
    line.push_str(req.module_path);
    if !raw_args.is_empty() {
        line.push(' ');
        line.push_str(&raw_args);
    }
    line.push(';');
    line
}

pub struct PerlSyntax;

impl ImportSyntax for PerlSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_perl_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_perl_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_perl(module_path)
    }
}

pub static PERL_SYNTAX: PerlSyntax = PerlSyntax;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_perl(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Perl);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Perl);
        (tree, block)
    }

    fn structured(import: &ImportStatement) -> (&[String], Option<&str>) {
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
                (modifiers, import_kind.as_deref())
            }
            other => panic!("expected Perl Structured import, got {other:?}"),
        }
    }

    /// Grammar fixture: lock the exact tree-sitter-perl node kinds this parser
    /// depends on. This grammar represents every `use`/`no` pragma (plain or
    /// specialized like `use parent` / `use constant`) as a single
    /// `use_statement` with a `module` field and a leading `use`/`no` keyword
    /// token, and runtime `require` as an `expression_statement` wrapping a
    /// `require_expression`.
    #[test]
    fn perl_grammar_node_kinds_are_stable() {
        let src = "use Foo::Bar;\nuse Foo qw(a b);\nuse parent -norequire, 'Base';\nuse constant PI => 3.14;\nrequire Foo;\nno warnings;\nno strict 'refs';\n";
        let (tree, _) = parse_perl(src);
        assert!(!tree.root_node().has_error());

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
            "source_file",
            "use_statement",
            "expression_statement",
            "require_expression",
            "package",
            "use",
            "no",
            ";",
        ] {
            assert!(
                kinds.contains(required),
                "perl grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_perl_supported_forms() {
        let (_, block) = parse_perl(
            "use Foo::Bar;\nuse Foo qw(a b);\nuse parent -norequire, 'Base';\nuse constant PI => 3.14;\nrequire Foo;\nno warnings;\nno strict 'refs';\n",
        );
        assert_eq!(block.imports.len(), 7);

        assert_perl_import(&block.imports[0], "Foo::Bar", PERL_USE_KIND, "");
        assert_perl_import(&block.imports[1], "Foo", PERL_USE_KIND, "qw(a b)");
        assert_perl_import(
            &block.imports[2],
            "parent",
            PERL_USE_KIND,
            "-norequire, 'Base'",
        );
        assert_perl_import(&block.imports[3], "constant", PERL_USE_KIND, "PI => 3.14");
        assert_perl_import(&block.imports[4], "Foo", PERL_REQUIRE_KIND, "");
        assert_perl_import(&block.imports[5], "warnings", PERL_NO_KIND, "");
        assert_perl_import(&block.imports[6], "strict", PERL_NO_KIND, "'refs'");
    }

    fn assert_perl_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_import_kind: &str,
        expected_raw_args: &str,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert!(imp.default_import.is_some());
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::SideEffect);
        assert_eq!(imp.group, ImportGroup::External);

        let marker = imp.default_import.as_deref().unwrap();
        assert_eq!(
            perl_marker_parts(marker),
            Some((expected_import_kind, expected_raw_args))
        );

        let (modifiers, import_kind) = structured(imp);
        assert_eq!(import_kind, Some(expected_import_kind));
        if expected_raw_args.is_empty() {
            assert!(modifiers.is_empty());
        } else {
            assert_eq!(modifiers, &[expected_raw_args.to_string()]);
        }
    }

    #[test]
    fn generate_perl_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy("Foo::Bar", &[], None, None, false)
            ),
            "use Foo::Bar;"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["qw(a b)".to_string()],
                    import_kind: Some(PERL_USE_KIND),
                }
            ),
            "use Foo qw(a b);"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some(PERL_REQUIRE_KIND),
                }
            ),
            "require Foo;"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "strict",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["'refs'".to_string()],
                    import_kind: Some(PERL_NO_KIND),
                }
            ),
            "no strict 'refs';"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "parent",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["-norequire, 'Base'".to_string()],
                    import_kind: Some(PERL_USE_KIND),
                }
            ),
            "use parent -norequire, 'Base';"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: "constant",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &["PI => 3.14".to_string()],
                    import_kind: Some(PERL_USE_KIND),
                }
            ),
            "use constant PI => 3.14;"
        );
    }

    #[test]
    fn generate_perl_preserves_organized_flat_markers() {
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy(
                    "Foo",
                    &[],
                    Some(&perl_flat_marker(PERL_REQUIRE_KIND, "")),
                    None,
                    false,
                )
            ),
            "require Foo;"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy(
                    "strict",
                    &[],
                    Some(&perl_flat_marker(PERL_NO_KIND, "'refs'")),
                    None,
                    false,
                )
            ),
            "no strict 'refs';"
        );
        assert_eq!(
            generate_import(
                LangId::Perl,
                &ImportRequest::legacy(
                    "Foo",
                    &[],
                    Some(&perl_flat_marker(PERL_USE_KIND, "qw(a b)")),
                    None,
                    false,
                )
            ),
            "use Foo qw(a b);"
        );
    }

    #[test]
    fn classify_group_perl_is_neutral_external() {
        assert_eq!(classify_group_perl("strict"), ImportGroup::External);
        assert_eq!(classify_group_perl("warnings"), ImportGroup::External);
        assert_eq!(classify_group_perl("Foo::Bar"), ImportGroup::External);
    }

    #[test]
    fn perl_round_trips_through_parse_generate() {
        for src in [
            "use Foo::Bar;",
            "use Foo qw(a b);",
            "use parent -norequire, 'Base';",
            "use constant PI => 3.14;",
            "require Foo;",
            "no warnings;",
            "no strict 'refs';",
        ] {
            let (_, block) = parse_perl(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (modifiers, import_kind) = structured(imp);
            let regenerated = generate_import(
                LangId::Perl,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: imp.default_import.as_deref(),
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers,
                    import_kind,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
