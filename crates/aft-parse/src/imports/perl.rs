use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

pub const PERL_USE_KIND: &str = "use";
test_support_visibility! {
const PERL_REQUIRE_KIND: &str = "require";
}
pub const PERL_NO_KIND: &str = "no";
const PERL_FLAT_MARKER_PREFIX: &str = "perl:";

pub fn classify_group_perl(_module_path: &str) -> ImportGroup {
    // Perl pragmas/modules do not have a source-level stdlib/external/internal
    // grouping convention, so keep grouping neutral and stable.
    ImportGroup::External
}

pub fn parse_perl_imports(source: &str, tree: &Tree) -> ImportBlock {
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

test_support_visibility! {
fn perl_flat_marker(import_kind: &str, raw_args: &str) -> String {
    format!(
        "{PERL_FLAT_MARKER_PREFIX}{}:{import_kind}{raw_args}",
        import_kind.len()
    )
}
}

test_support_visibility! {
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

pub fn generate_perl_import_line(req: &ImportRequest) -> String {
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
