//! Concrete Go receiver bindings for the source-backed SQLite dispatch pass.
use super::*;
use tree_sitter::Node;

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn nodes(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        result.push(node);
        push_named_children(node, &mut stack);
    }
    result
}

fn nominal_type(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "type_identifier" => Some(text(node, source).to_string()),
        "pointer_type" => nominal_type(node.named_child(0)?, source),
        _ => None,
    }
}

fn package(parsed: &ParsedDispatchSource) -> Option<String> {
    let root = parsed.tree.root_node();
    let clause = root
        .named_children(&mut root.walk())
        .find(|n| n.kind() == "package_clause")?;
    Some(text(clause.named_child(0)?, &parsed.source).to_string())
}

fn contains(outer: Node<'_>, inner: Node<'_>) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}

fn scope(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = node.parent()?;
        if matches!(
            node.kind(),
            "block"
                | "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "communication_case"
                | "expression_case"
                | "type_case"
                | "function_declaration"
                | "method_declaration"
                | "func_literal"
        ) {
            return Some(node);
        }
    }
}

fn binding_names(node: Node<'_>, source: &str) -> Vec<String> {
    match node.kind() {
        "parameter_declaration" | "variadic_parameter_declaration" | "var_spec" => {
            let mut cursor = node.walk();
            node.children_by_field_name("name", &mut cursor)
                .map(|n| text(n, source).to_string())
                .collect()
        }
        "short_var_declaration" | "range_clause" | "receive_statement" => node
            .child_by_field_name("left")
            .map(|n| {
                n.named_children(&mut n.walk())
                    .map(|n| text(n, source).to_string())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn initializer_type(
    mut node: Node<'_>,
    function: Node<'_>,
    root: Node<'_>,
    source: &str,
) -> Option<String> {
    while matches!(node.kind(), "parenthesized_expression" | "unary_expression") {
        if node.kind() == "unary_expression"
            && node
                .child_by_field_name("operator")
                .map(|n| text(n, source))
                != Some("&")
        {
            return None;
        }
        node = node.named_child(0)?;
    }
    if node.kind() == "composite_literal" {
        return nominal_type(node.child_by_field_name("type")?, source);
    }
    if node.kind() != "call_expression" {
        return None;
    }
    let callee = node.child_by_field_name("function")?;
    if callee.kind() != "identifier" {
        return None;
    }
    let name = text(callee, source);
    // A local function value must not borrow the return type of a homonymous
    // package function. Only a uniquely declared, unshadowed function is evidence.
    if nodes(function).iter().any(|n| {
        binding_names(*n, source)
            .iter()
            .any(|binding| binding == name)
    }) {
        return None;
    }
    let mut cursor = root.walk();
    let mut functions = root.named_children(&mut cursor).filter(|n| {
        n.kind() == "function_declaration"
            && n.child_by_field_name("name").map(|n| text(n, source)) == Some(name)
    });
    let declared = functions.next()?;
    if functions.next().is_some() || declared.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    nominal_type(declared.child_by_field_name("result")?, source)
}

fn receiver_type(call: Node<'_>, parsed: &ParsedDispatchSource, receiver: &str) -> Option<String> {
    let source = &parsed.source;
    let mut function = call.parent()?;
    while !matches!(
        function.kind(),
        "function_declaration" | "method_declaration"
    ) {
        function = function.parent()?;
    }
    let all = nodes(function);
    // Refuse shadowed bindings rather than choosing a declaration by spelling.
    // This also makes captures with a redeclared closure parameter conservative.
    let mut bindings = all.iter().copied().filter(|n| {
        binding_names(*n, source)
            .iter()
            .any(|name| name == receiver)
    });
    let declaration = bindings.next()?;
    if bindings.next().is_some() || !contains(scope(declaration)?, call) {
        return None;
    }
    let ty = if let Some(ty) = declaration.child_by_field_name("type") {
        nominal_type(ty, source)?
    } else {
        if declaration.end_byte() > call.start_byte() {
            return None;
        }
        let names = binding_names(declaration, source);
        let value = declaration
            .child_by_field_name("right")
            .or_else(|| declaration.child_by_field_name("value"))?;
        let values = value.named_children(&mut value.walk()).collect::<Vec<_>>();
        if names.len() != values.len() {
            return None;
        }
        let position = names.iter().position(|name| name == receiver)?;
        initializer_type(values[position], function, parsed.tree.root_node(), source)?
    };
    // Local types and type parameters can shadow the package's nominal type.
    if all.iter().any(|n| {
        matches!(
            n.kind(),
            "type_spec" | "type_alias" | "type_parameter_declaration"
        ) && n
            .children_by_field_name("name", &mut n.walk())
            .any(|n| text(n, source) == ty)
    }) {
        return None;
    }
    Some(ty)
}

/// `None` means no supported binding; `Some(None)` means a bound receiver whose
/// method is absent or ambiguous, so name-only fallback must not replace it.
pub(super) fn resolve(
    project_root: &Path,
    reference: &NameMatchRef,
    candidates: &[NameMatchCandidate],
    source_cache: &mut DispatchSourceCache,
) -> Option<Option<NameMatchCandidate>> {
    if reference.colon_dispatch || !receiver_is_bare_identifier(&reference.receiver_expression) {
        return None;
    }
    let parsed = parsed_dispatch_source(project_root, reference, LangId::Go, source_cache)?;
    let package = package(parsed)?;
    let mut types = BTreeSet::new();
    for call in nodes(parsed.tree.root_node()).into_iter().filter(|n| {
        n.kind() == "call_expression" && n.start_position().row as u32 + 1 == reference.line
    }) {
        let Some(callee) = call.child_by_field_name("function") else {
            continue;
        };
        if callee.kind() != "selector_expression"
            || callee
                .child_by_field_name("operand")
                .map(|n| text(n, &parsed.source))
                != Some(reference.receiver_expression.as_str())
            || callee
                .child_by_field_name("field")
                .map(|n| text(n, &parsed.source))
                != Some(reference.method_name.as_str())
        {
            continue;
        }
        types.insert(receiver_type(call, parsed, &reference.receiver_expression));
    }
    if types.len() != 1 {
        return None;
    }
    let ty = types.into_iter().next()??;
    let scoped = format!("{ty}::{}", reference.method_name);
    let directory = Path::new(&reference.caller_file).parent();
    let mut found = Vec::new();
    for candidate in candidates.iter().filter(|candidate| {
        candidate.scoped_name == scoped
            && Path::new(&candidate.file_path).parent() == directory
            && (reference.caller_file.ends_with("_test.go")
                || !candidate.file_path.ends_with("_test.go"))
    }) {
        let Some(parsed) = parsed_dispatch_source_for_file(
            project_root,
            &candidate.file_path,
            "go",
            LangId::Go,
            source_cache,
        ) else {
            continue;
        };
        if self::package(parsed).as_deref() != Some(&package) {
            continue;
        }
        // A real receiver declaration proves a concrete method. An interface's
        // compatible method set is not a reverse link to this concrete type.
        let declared = nodes(parsed.tree.root_node()).into_iter().any(|node| {
            node.kind() == "method_declaration"
                && node
                    .child_by_field_name("name")
                    .map(|n| text(n, &parsed.source))
                    == Some(reference.method_name.as_str())
                && node
                    .child_by_field_name("receiver")
                    .and_then(|n| n.named_child(0))
                    .and_then(|n| n.child_by_field_name("type"))
                    .and_then(|n| nominal_type(n, &parsed.source))
                    .as_deref()
                    == Some(&ty)
        });
        if declared {
            found.push(candidate.clone());
        }
    }
    Some(if found.len() == 1 { found.pop() } else { None })
}
