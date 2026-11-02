//! Object identities describe literal declarations, not structural lookalikes.
//! Only lexical bindings and functions with a single explicit object-literal
//! return are treated as referring to one of these identities. Callback
//! arguments are not guessed by name.
use super::*;

pub(super) fn supported(language: &str) -> bool {
    matches!(language, "typescript" | "tsx" | "javascript")
}

pub(super) fn object_identity(object: Node<'_>) -> String {
    let position = object.start_position();
    format!("@object:{}:{}", position.row, position.column)
}

fn unwrap(mut node: Node<'_>) -> Node<'_> {
    loop {
        let child = match node.kind() {
            "parenthesized_expression"
            | "satisfies_expression"
            | "as_expression"
            | "non_null_expression" => node.named_child(0),
            "type_assertion" => node.named_children(&mut node.walk()).last(),
            _ => None,
        };
        match child {
            Some(child) => node = child,
            None => return node,
        }
    }
}

pub(super) fn object_member<'a>(node: Node<'a>, source: &str) -> Option<(Node<'a>, String)> {
    let (object, key) = if node.kind() == "method_definition" {
        if node
            .children(&mut node.walk())
            .any(|n| matches!(n.kind(), "get" | "set"))
        {
            return None;
        }
        (node.parent()?, field(node, &["name"])?)
    } else if matches!(
        node.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    ) {
        let pair = node.parent().filter(|p| p.kind() == "pair")?;
        if field(pair, &["value"])? != node {
            return None;
        }
        (pair.parent()?, field(pair, &["key"])?)
    } else {
        return None;
    };
    if object.kind() != "object" || !matches!(key.kind(), "property_identifier" | "identifier") {
        return None;
    }
    Some((object, text(key, source).to_string()))
}

fn scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "program" | "statement_block" | "for_statement" | "for_in_statement" | "catch_clause"
    ) || FUNCTIONS.contains(&node.kind())
}

fn binding_scope(node: Node<'_>) -> Option<Node<'_>> {
    let mut parent = node.parent()?;
    loop {
        if scope(parent) {
            return Some(parent);
        }
        parent = parent.parent()?;
    }
}

pub(super) struct Objects<'nodes, 'tree, 'source> {
    nodes: &'nodes [Node<'tree>],
    source: &'source str,
    symbols: HashMap<(usize, usize), String>,
    returns: HashMap<String, String>,
    imports: HashSet<String>,
}

impl<'nodes, 'tree, 'source> Objects<'nodes, 'tree, 'source> {
    pub(super) fn new(
        nodes: &'nodes [Node<'tree>],
        source: &'source str,
        parse: &ParseBlob,
    ) -> Self {
        let mut result = Self {
            nodes,
            source,
            symbols: HashMap::new(),
            returns: HashMap::new(),
            imports: parse
                .imports
                .iter()
                .flat_map(|i| {
                    i.names
                        .iter()
                        .filter_map(|n| n.split_whitespace().last().map(str::to_owned))
                        .chain(i.default_import.iter().cloned())
                })
                .collect(),
        };
        for node in nodes.iter().filter(|n| FUNCTIONS.contains(&n.kind())) {
            if let Some(symbol) = parse.symbols.iter().find(|s| {
                s.end_line == node.end_position().row as u32
                    && s.end_col == node.end_position().column as u32
                    && matches!(s.kind.as_str(), "function" | "method")
            }) {
                result.symbols.insert(
                    (node.start_byte(), node.end_byte()),
                    symbol.scoped_name.clone(),
                );
                if let Some(object) = result.returned_object(*node) {
                    result
                        .returns
                        .insert(symbol.scoped_name.clone(), object_identity(object));
                }
            }
        }
        result
    }

    fn returned_object(&self, function: Node<'tree>) -> Option<Node<'tree>> {
        // An async factory returns a Promise, not the literal itself.
        if function
            .children(&mut function.walk())
            .any(|n| n.kind() == "async")
        {
            return None;
        }
        let body = field(function, &["body"])?;
        let expression = if body.kind() == "statement_block" {
            let returns = self
                .nodes
                .iter()
                .filter(|n| {
                    n.kind() == "return_statement" && enclosing(**n, FUNCTIONS) == Some(function)
                })
                .copied()
                .collect::<Vec<_>>();
            if returns.len() != 1 || returns[0].parent() != Some(body) {
                return None;
            }
            returns[0].named_child(0)?
        } else {
            body
        };
        let object = unwrap(expression);
        (object.kind() == "object").then_some(object)
    }

    pub(super) fn unambiguous_member(&self, object: Node<'tree>, member: &str) -> bool {
        let mut count = 0;
        for child in object.named_children(&mut object.walk()) {
            if child.kind() == "spread_element" {
                return false;
            }
            if let Some(key) = field(child, &["key", "name"]) {
                if key.kind() == "computed_property_name" {
                    return false;
                }
                if text(key, self.source).trim_matches(['\'', '"']) == member {
                    count += 1;
                }
            }
        }
        count == 1
    }

    pub(super) fn extract(&self, facts: &mut DispatchFacts) {
        for &object in self.nodes.iter().filter(|n| n.kind() == "object") {
            facts.types.push(TypeHint {
                name: object_identity(object),
                bases: Vec::new(),
                interface: false,
                closed: true,
            });
            for pair in object
                .named_children(&mut object.walk())
                .filter(|n| n.kind() == "pair")
            {
                let Some(key) = field(pair, &["key"]) else {
                    continue;
                };
                if !matches!(key.kind(), "property_identifier" | "identifier") {
                    continue;
                }
                let name = text(key, self.source);
                if !self.unambiguous_member(object, name) {
                    continue;
                }
                if let Some(value) = field(pair, &["value"])
                    .map(unwrap)
                    .filter(|n| n.kind() == "object")
                {
                    facts.fields.push(FieldHint {
                        owner: object_identity(object),
                        name: name.to_string(),
                        ty: object_identity(value),
                    });
                }
            }
        }
        facts
            .returns
            .extend(self.returns.iter().map(|(symbol, ty)| ReturnHint {
                symbol: symbol.clone(),
                ty: ty.clone(),
            }));
    }

    fn binding(&self, identifier: Node<'tree>) -> Option<Node<'tree>> {
        let name = text(identifier, self.source);
        let mut current = identifier.parent();
        while let Some(ancestor) = current {
            if scope(ancestor) {
                let candidates = self
                    .nodes
                    .iter()
                    .filter(|n| {
                        matches!(
                            n.kind(),
                            "variable_declarator"
                                | "required_parameter"
                                | "optional_parameter"
                                | "function_declaration"
                                | "class_declaration"
                                | "catch_clause"
                        ) && field(**n, &["name", "pattern", "parameter"])
                            .is_some_and(|key| text(key, self.source) == name)
                            && binding_scope(**n) == Some(ancestor)
                    })
                    .copied()
                    .collect::<Vec<_>>();
                if !candidates.is_empty() {
                    return (candidates.len() == 1).then_some(candidates[0]);
                }
            }
            current = ancestor.parent();
        }
        None
    }

    fn written(&self, binding: Node<'tree>, name: &str) -> bool {
        let Some(scope) = binding_scope(binding) else {
            return true;
        };
        self.nodes.iter().any(|node| {
            if !matches!(
                node.kind(),
                "assignment_expression"
                    | "augmented_assignment_expression"
                    | "update_expression"
                    | "unary_expression"
            ) || node.start_byte() < scope.start_byte()
                || node.end_byte() > scope.end_byte()
            {
                return false;
            }
            field(*node, &["left", "argument"]).is_some_and(|left| {
                let lhs = text(left, self.source);
                lhs == name
                    || lhs
                        .strip_prefix(name)
                        .is_some_and(|tail| tail.starts_with('.') || tail.starts_with('['))
            })
        })
    }

    pub(super) fn receiver(&self, expression: Node<'tree>, call: Node<'tree>) -> Option<String> {
        self.value(expression, call, 0)
    }

    fn value(&self, expression: Node<'tree>, call: Node<'tree>, depth: usize) -> Option<String> {
        if depth > 16 {
            return None;
        }
        let expression = unwrap(expression);
        match expression.kind() {
            "object" => Some(object_identity(expression)),
            "identifier" => {
                let binding = self.binding(expression)?;
                let name = text(expression, self.source);
                if binding.kind() != "variable_declarator"
                    || binding.start_byte() >= call.start_byte()
                    || self.written(binding, name)
                {
                    return None;
                }
                self.value(field(binding, &["value"])?, call, depth + 1)
            }
            "member_expression" => {
                let base = self.value(field(expression, &["object"])?, call, depth + 1)?;
                let member = text(field(expression, &["property"])?, self.source);
                Some(format!("@field:{base}|{member}"))
            }
            "call_expression" => {
                let callee = field(expression, &["function"])?;
                if callee.kind() != "identifier" {
                    return None;
                }
                let name = text(callee, self.source);
                if let Some(binding) = self.binding(callee) {
                    if self.written(binding, name) {
                        return None;
                    }
                    let function = if binding.kind() == "function_declaration" {
                        binding
                    } else if binding.kind() == "variable_declarator" {
                        unwrap(field(binding, &["value"])?)
                    } else {
                        return None;
                    };
                    let symbol = self
                        .symbols
                        .get(&(function.start_byte(), function.end_byte()))?;
                    self.returns.get(symbol).cloned()
                } else if self.imports.contains(name) {
                    Some(format!("@return:{name}"))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}
