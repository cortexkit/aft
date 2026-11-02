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
    end: usize,
    local: bool,
    ty: Option<String>,
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
    match node.kind() {
        "parameter_declaration" | "variadic_parameter_declaration" | "var_spec" | "const_spec" => {
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
            let mut declarations = HashMap::<String, Option<Node<'_>>>::new();
            let mut type_shadows = HashSet::new();
            for &node in all {
                for name in binding_names(node, source) {
                    declarations
                        .entry(name)
                        .and_modify(|old| *old = None)
                        .or_insert(Some(node));
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
                let Some(declaration) = declaration else {
                    continue;
                };
                if declaration.kind() == "variadic_parameter_declaration" {
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

/// Select a uniquely declared method in the receiver's own Go package.
pub(super) fn resolve(
    project_root: &Path,
    reference: &NameMatchRef,
    candidates: &[NameMatchCandidate],
    source_cache: &mut DispatchSourceCache,
) -> Option<NameMatchCandidate> {
    if reference.colon_dispatch || !receiver_is_bare_identifier(&reference.receiver_expression) {
        return None;
    }
    let directory = Path::new(&reference.caller_file).parent();
    // A type defined in the caller's Go package cannot acquire methods from
    // another directory.
    // Avoid parsing callers for library-only names such as testing.T helpers.
    if !candidates.iter().any(|candidate| {
        Path::new(&candidate.file_path).parent() == directory
            && (reference.caller_file.ends_with("_test.go")
                || !candidate.file_path.ends_with("_test.go"))
    }) {
        return None;
    }
    let parsed = parsed_dispatch_source(project_root, reference, LangId::Go, source_cache)?;
    let index = parsed.go_index.as_ref()?;
    let package = index.package.as_ref()?.clone();
    let ty = index
        .calls_by_line
        .get(&reference.line)?
        .get(&(
            reference.receiver_expression.clone(),
            reference.method_name.clone(),
        ))?
        .as_ref()?
        .clone();
    let scoped = format!("{ty}::{}", reference.method_name);
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
        let Some(index) = parsed.go_index.as_ref() else {
            continue;
        };
        if index.package.as_ref() != Some(&package) {
            continue;
        }
        // A real receiver declaration proves a concrete method. An interface's
        // compatible method set is not a reverse link to this concrete type.
        let declared = index
            .methods
            .contains(&(ty.clone(), reference.method_name.clone()));
        if declared {
            found.push(candidate.clone());
        }
    }
    if found.len() == 1 {
        found.pop()
    } else {
        None
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
}
