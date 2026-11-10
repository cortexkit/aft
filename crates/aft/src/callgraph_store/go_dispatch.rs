//! Indexed Go receiver evidence shared by fact extraction and SQLite dispatch.
use super::*;
use tree_sitter::Node;

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

#[derive(Default)]
pub(super) struct SourceIndex {
    pub(super) package: Option<String>,
    calls: HashMap<(usize, usize), Option<String>>,
    calls_by_line: HashMap<u32, HashMap<(String, String), Option<String>>>,
    methods: HashSet<(String, String)>,
}

struct Binding {
    scope: Option<(usize, usize)>,
    repeated_scopes: Option<Vec<(usize, usize)>>,
    end: usize,
    local: bool,
    ty: Option<String>,
}

enum Declarations<'tree> {
    One(Node<'tree>),
    Repeated(Vec<Node<'tree>>),
}

#[cfg(test)]
thread_local! {
    static WORK: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) };
}

fn nominal_type(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "type_identifier" => Some(text(node, source).to_string()),
        "pointer_type" if node.named_child(0)?.kind() == "type_identifier" => {
            nominal_type(node.named_child(0)?, source)
        }
        _ => None,
    }
}

fn package(root: Node<'_>, source: &str) -> Option<String> {
    let clause = root
        .named_children(&mut root.walk())
        .find(|n| n.kind() == "package_clause")?;
    Some(text(clause.named_child(0)?, source).to_string())
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
    if matches!(
        node.kind(),
        "parameter_declaration" | "variadic_parameter_declaration"
    ) {
        // Function-type signatures name their arguments, not values in the
        // enclosing function's scope.
        let mut parent = node.parent();
        while let Some(node) = parent {
            if node.kind() == "function_type" {
                return Vec::new();
            }
            if matches!(
                node.kind(),
                "function_declaration" | "method_declaration" | "func_literal"
            ) {
                break;
            }
            parent = node.parent();
        }
    }
    match node.kind() {
        "parameter_declaration"
        | "variadic_parameter_declaration"
        | "var_spec"
        | "const_spec"
        | "type_spec"
        | "type_alias"
        | "type_parameter_declaration" => {
            let mut cursor = node.walk();
            node.children_by_field_name("name", &mut cursor)
                .map(|n| text(n, source).to_string())
                .collect()
        }
        "short_var_declaration"
        | "range_clause"
        | "receive_statement"
        | "type_switch_statement" => node
            .child_by_field_name("left")
            .or_else(|| node.child_by_field_name("alias"))
            .map(|n| {
                n.named_children(&mut n.walk())
                    .map(|n| text(n, source).to_string())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn repeated_parameter_binding(
    declarations: &[Node<'_>],
    source: &str,
    type_shadows: &HashSet<String>,
) -> Option<Binding> {
    let mut ty = None;
    let mut scopes = Vec::new();
    let mut other_scopes = Vec::new();
    for &declaration in declarations {
        let declaration_scope = scope(declaration)?;
        if declaration.kind() == "variadic_parameter_declaration"
            && declaration_scope.kind() == "func_literal"
        {
            return None;
        }
        if declaration.kind() != "parameter_declaration"
            || declaration_scope.kind() != "func_literal"
        {
            other_scopes.push((declaration_scope.start_byte(), declaration_scope.end_byte()));
            continue;
        }
        let scope = declaration_scope.child_by_field_name("body")?;
        let declared = nominal_type(declaration.child_by_field_name("type")?, source)?;
        if type_shadows.contains(&declared) || ty.as_ref().is_some_and(|ty| ty != &declared) {
            return None;
        }
        ty = Some(declared);
        scopes.push((scope.start_byte(), scope.end_byte()));
    }
    if scopes.len() < 2 {
        return None;
    }
    scopes.sort_unstable();
    // Only disjoint closures qualify. A call must be covered by its own
    // parameter's scope, never by a same-named parameter in another callback.
    if scopes.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return None;
    }
    // An unrelated local may use the name elsewhere, but must neither shadow
    // a callback parameter nor borrow its type outside that callback's body.
    for (start, end) in other_scopes {
        let position = scopes.partition_point(|(_, scope_end)| *scope_end <= start);
        if position < scopes.len() && scopes[position].0 < end {
            return None;
        }
    }
    Some(Binding {
        scope: None,
        repeated_scopes: Some(scopes),
        end: 0,
        local: false,
        ty,
    })
}

fn initializer_type(
    mut node: Node<'_>,
    source: &str,
    bound_names: &HashSet<String>,
    factories: &HashMap<String, Option<String>>,
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
    if bound_names.contains(name) {
        return None;
    }
    factories.get(name).cloned().flatten()
}

impl SourceIndex {
    pub(super) fn new(root: Node<'_>, source: &str, by_line: bool) -> Self {
        let mut index = Self {
            package: package(root, source),
            ..Self::default()
        };
        let mut factories = HashMap::new();
        let mut has_dot_import = false;
        let mut functions = HashMap::<usize, Vec<Node<'_>>>::new();
        let mut stack = vec![(root, None)];
        // Walk each source once. Function literals stay in their enclosing
        // declaration so captured names and closure-parameter shadows agree.
        while let Some((node, function)) = stack.pop() {
            #[cfg(test)]
            WORK.with(|work| {
                let (n, f, b) = work.get();
                work.set((n + 1, f, b));
            });
            let function = if matches!(node.kind(), "function_declaration" | "method_declaration") {
                Some(node.start_byte())
            } else {
                function
            };
            if let Some(function) = function {
                functions.entry(function).or_default().push(node);
            }
            if node.kind() == "import_spec"
                && node
                    .child_by_field_name("name")
                    .is_some_and(|name| text(name, source) == ".")
            {
                has_dot_import = true;
            }
            if node.kind() == "function_declaration" {
                if let Some(name) = node.child_by_field_name("name") {
                    let ty = node
                        .child_by_field_name("result")
                        .filter(|_| node.child_by_field_name("type_parameters").is_none())
                        .and_then(|n| nominal_type(n, source));
                    factories
                        .entry(text(name, source).to_string())
                        .and_modify(|old| *old = None)
                        .or_insert(ty);
                }
            }
            if node.kind() == "method_declaration" {
                if let (Some(name), Some(ty)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("receiver")
                        .and_then(|n| n.named_child(0))
                        .and_then(|n| n.child_by_field_name("type"))
                        .and_then(|n| nominal_type(n, source)),
                ) {
                    index.methods.insert((ty, text(name, source).to_string()));
                }
            }
            stack.extend(
                node.named_children(&mut node.walk())
                    .map(|child| (child, function)),
            );
        }
        for all in functions.values() {
            #[cfg(test)]
            WORK.with(|work| {
                let (n, f, b) = work.get();
                work.set((n, f + 1, b));
            });
            let mut declarations = HashMap::<String, Declarations<'_>>::new();
            let mut type_shadows = HashSet::new();
            for &node in all {
                for name in binding_names(node, source) {
                    if name == "_" {
                        continue;
                    }
                    declarations
                        .entry(name)
                        .and_modify(|old| match old {
                            Declarations::One(first) => {
                                *old = Declarations::Repeated(vec![*first, node])
                            }
                            Declarations::Repeated(all) => all.push(node),
                        })
                        .or_insert(Declarations::One(node));
                }
                if matches!(
                    node.kind(),
                    "type_spec" | "type_alias" | "type_parameter_declaration"
                ) {
                    type_shadows.extend(
                        node.children_by_field_name("name", &mut node.walk())
                            .map(|n| text(n, source).to_string()),
                    );
                }
            }
            let bound_names = declarations.keys().cloned().collect::<HashSet<_>>();
            let mut bindings = HashMap::new();
            for (name, declaration) in declarations {
                #[cfg(test)]
                WORK.with(|work| {
                    let (n, f, b) = work.get();
                    work.set((n, f, b + 1));
                });
                let declaration = match declaration {
                    Declarations::One(node) => node,
                    Declarations::Repeated(all) => {
                        // A dot import may shadow an unqualified type name;
                        // its export set is not resolved by this source index.
                        if !has_dot_import {
                            if let Some(binding) =
                                repeated_parameter_binding(&all, source, &type_shadows)
                            {
                                bindings.insert(name, binding);
                            }
                        }
                        continue;
                    }
                };
                if matches!(
                    declaration.kind(),
                    "variadic_parameter_declaration"
                        | "type_spec"
                        | "type_alias"
                        | "type_parameter_declaration"
                ) {
                    continue;
                }
                let ty = declaration
                    .child_by_field_name("type")
                    .and_then(|n| nominal_type(n, source))
                    .or_else(|| {
                        if declaration.child_by_field_name("type").is_some() {
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
                        initializer_type(
                            values[names.iter().position(|n| n == &name)?],
                            source,
                            &bound_names,
                            &factories,
                        )
                    })
                    .filter(|ty| !type_shadows.contains(ty));
                bindings.insert(
                    name,
                    Binding {
                        scope: scope(declaration).map(|n| (n.start_byte(), n.end_byte())),
                        repeated_scopes: None,
                        end: declaration.end_byte(),
                        local: !matches!(
                            declaration.kind(),
                            "parameter_declaration" | "variadic_parameter_declaration"
                        ),
                        ty,
                    },
                );
            }
            for &call in all.iter().filter(|n| n.kind() == "call_expression") {
                let Some(callee) = call
                    .child_by_field_name("function")
                    .filter(|n| n.kind() == "selector_expression")
                else {
                    continue;
                };
                let (Some(receiver), Some(member)) = (
                    callee.child_by_field_name("operand"),
                    callee.child_by_field_name("field"),
                ) else {
                    continue;
                };
                if receiver.kind() != "identifier" {
                    continue;
                }
                let receiver = text(receiver, source).to_string();
                let ty = bindings.get(&receiver).and_then(|b| {
                    if let Some(scopes) = &b.repeated_scopes {
                        let position =
                            scopes.partition_point(|(start, _)| *start <= call.start_byte());
                        return (position > 0 && call.end_byte() <= scopes[position - 1].1)
                            .then(|| b.ty.clone())
                            .flatten();
                    }
                    let (start, end) = b.scope?;
                    (start <= call.start_byte()
                        && call.end_byte() <= end
                        && (!b.local || b.end <= call.start_byte()))
                    .then(|| b.ty.clone())
                    .flatten()
                });
                let span = (call.start_byte(), call.end_byte());
                if by_line {
                    index
                        .calls_by_line
                        .entry(call.start_position().row as u32 + 1)
                        .or_default()
                        .entry((receiver, text(member, source).to_string()))
                        .and_modify(|old| {
                            if *old != ty {
                                *old = None;
                            }
                        })
                        .or_insert(ty);
                } else {
                    index.calls.insert(span, ty);
                }
            }
        }
        index
    }

    pub(super) fn receiver(&self, start: usize, end: usize) -> Option<&str> {
        self.calls.get(&(start, end))?.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_concrete_index_work_is_linear_and_lookups_do_not_walk_source() {
        let mut visits = Vec::new();
        for count in [40, 160] {
            let source = format!("package p\ntype T struct{{}}\nfunc (x *T) run() {{}}\nfunc factory() *T {{ return &T{{}} }}\nfunc caller() {{\n x := factory()\n{}\n}}\n", "x.run()\n".repeat(count));
            let tree =
                parse_source_with_cached_parser(Path::new("indexed.go"), &source, LangId::Go)
                    .unwrap();
            WORK.with(|work| work.set((0, 0, 0)));
            let index = SourceIndex::new(tree.root_node(), &source, false);
            let work = WORK.with(|work| work.get());
            assert_eq!(work.1, 3, "analyze each function once, not once per call");
            assert_eq!(work.2, 2, "receiver and local bindings are analyzed once");
            assert_eq!(index.calls.len(), count);
            for (&(start, end), ty) in &index.calls {
                assert_eq!(ty.as_deref(), Some("T"));
                assert_eq!(index.receiver(start, end), Some("T"));
            }
            assert!(index.calls_by_line.is_empty());
            assert_eq!(
                WORK.with(|work| work.get()),
                work,
                "span lookups must not walk the source or function"
            );
            WORK.with(|work| work.set((0, 0, 0)));
            let lines = SourceIndex::new(tree.root_node(), &source, true);
            assert!(lines.calls.is_empty());
            assert_eq!(WORK.with(|work| work.get()), work);
            for calls in lines.calls_by_line.values() {
                assert_eq!(
                    calls.get(&("x".into(), "run".into())).unwrap().as_deref(),
                    Some("T")
                );
            }
            assert_eq!(
                WORK.with(|work| work.get()),
                work,
                "lookups must not walk the source or function"
            );
            visits.push(work.0);
        }
        assert!(
            visits[1] < visits[0] * 5,
            "four times as many calls must have linear indexing work: {visits:?}"
        );
    }

    #[test]
    fn go_concrete_repeated_parameter_scope_index_has_bounded_lookup_work() {
        let mut visits = Vec::new();
        for count in [40, 160] {
            let source = format!("package p\ntype T struct{{}}\nfunc (r *T) run() {{}}\nfunc caller() {{ var callbacks=[]func(x *T){{\n{}\n}}; _=callbacks }}", "func(x *T) { x.run() },\n".repeat(count));
            let tree =
                parse_source_with_cached_parser(Path::new("callbacks.go"), &source, LangId::Go)
                    .unwrap();
            WORK.with(|work| work.set((0, 0, 0)));
            let index = SourceIndex::new(tree.root_node(), &source, false);
            let work = WORK.with(|work| work.get());
            assert_eq!(work.1, 2);
            assert_eq!(
                work.2, 3,
                "function-type parameter names are not value bindings"
            );
            assert_eq!(index.calls.len(), count);
            assert!(index.calls.values().all(|ty| ty.as_deref() == Some("T")));
            for &(start, end) in index.calls.keys() {
                assert_eq!(index.receiver(start, end), Some("T"));
            }
            assert_eq!(WORK.with(|work| work.get()), work);
            visits.push(work.0);
        }
        assert!(
            visits[1] < visits[0] * 5,
            "scope indexing must not rescan a function per callback: {visits:?}"
        );
    }
}
