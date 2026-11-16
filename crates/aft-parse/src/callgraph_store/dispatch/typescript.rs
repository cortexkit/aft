//! Object identities describe literal declarations, not structural lookalikes.
//! Only lexical bindings and functions with a single explicit object-literal
//! return are treated as referring to one of these identities. Callback
//! arguments are not guessed by name.
use super::*;
// The object indexes are only looked up, never iterated in an order that
// shows in the output, so a fast hash serves them.
use rustc_hash::{FxHashMap, FxHashSet};

pub(super) fn supported(language: &str) -> bool {
    matches!(language, "typescript" | "tsx" | "javascript")
}

pub(super) fn object_identity(object: Node<'_>) -> String {
    let position = object.start_position();
    format!("@object:{}:{}", position.row, position.column)
}

pub(super) fn complete_symbols<'tree>(
    nodes: &[Node<'tree>],
    source: &str,
    symbols: &mut Vec<crate::symbols::Symbol>,
    nav: impl Nav<'tree>,
) {
    use crate::symbols::{Symbol, SymbolKind};
    let mut identities = HashMap::new();
    for symbol in symbols.iter() {
        *identities
            .entry((symbol.scope_chain.clone(), symbol.name.clone()))
            .or_insert(0usize) += 1;
    }
    let mut positions = symbols
        .iter()
        .enumerate()
        .map(|(index, symbol)| {
            (
                (
                    symbol.range.end_line,
                    symbol.range.end_col,
                    symbol.name.clone(),
                ),
                index,
            )
        })
        .collect::<HashMap<_, _>>();
    for &node in nodes {
        if !matches!(
            nav.kind(node),
            "method_definition" | "arrow_function" | "function_expression" | "generator_function"
        ) {
            continue;
        }
        let Some((object, name)) = object_member(node, source, nav) else {
            continue;
        };
        let owner = object_identity(object);
        let definition = nav
            .parent(node)
            .filter(|p| nav.kind(*p) == "pair")
            .unwrap_or(node);
        let mut declarations = vec![(name.clone(), definition, SymbolKind::Method)];
        if let Some(alias) = field(node, &["name"]).filter(|_| node.kind() != "method_definition") {
            if text(alias, source) != name {
                declarations.push((text(alias, source).to_string(), node, SymbolKind::Function));
            }
        }
        for (name, definition, kind) in declarations {
            let key = (
                definition.end_position().row as u32,
                definition.end_position().column as u32,
                name.clone(),
            );
            if let Some(&index) = positions.get(&key) {
                let symbol = &mut symbols[index];
                // Preserve a previously selectable declaration unless its old
                // qualified identity combines different source declarations.
                if identities
                    .get(&(symbol.scope_chain.clone(), symbol.name.clone()))
                    .copied()
                    .unwrap_or_default()
                    > 1
                {
                    symbol.scope_chain = vec![owner.clone()];
                    symbol.parent = Some(owner.clone());
                }
                continue;
            }
            let header_end =
                field(node, &["body"]).map_or(definition.end_byte(), |body| body.start_byte());
            let header = source
                .get(definition.start_byte()..header_end)
                .unwrap_or_default();
            let mut signature = header
                .lines()
                .next()
                .unwrap_or(header)
                .trim_end()
                .to_string();
            crate::parser::truncate_signature(&mut signature);
            positions.insert(key, symbols.len());
            symbols.push(Symbol {
                name,
                kind,
                range: crate::parser::node_range_with_decorators_with(
                    &definition,
                    source,
                    LangId::TypeScript,
                    nav,
                ),
                signature: Some(signature),
                scope_chain: vec![owner.clone()],
                exported: false,
                parent: Some(owner.clone()),
            });
        }
    }
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

pub(super) fn object_member<'tree>(
    node: Node<'tree>,
    source: &str,
    nav: impl Nav<'tree>,
) -> Option<(Node<'tree>, String)> {
    let kind = nav.kind(node);
    let (object, key) = if kind == "method_definition" {
        if node
            .children(&mut node.walk())
            .any(|n| matches!(n.kind(), "get" | "set"))
        {
            return None;
        }
        (nav.parent(node)?, field(node, &["name"])?)
    } else if matches!(
        kind,
        "arrow_function" | "function_expression" | "generator_function"
    ) {
        let pair = nav.parent(node).filter(|p| nav.kind(*p) == "pair")?;
        if field(pair, &["value"])? != node {
            return None;
        }
        (nav.parent(pair)?, field(pair, &["key"])?)
    } else {
        return None;
    };
    if nav.kind(object) != "object" || !matches!(key.kind(), "property_identifier" | "identifier") {
        return None;
    }
    Some((object, text(key, source).to_string()))
}

pub(super) struct Objects<'nodes, 'tree, 'source, N> {
    nodes: &'nodes [Node<'tree>],
    source: &'source str,
    nav: N,
    returns: FxHashMap<String, String>,
    imports: FxHashSet<String>,
    import_node: Option<Node<'tree>>,
    bindings: FxHashMap<(usize, &'source str), Option<Node<'tree>>>,
    writes: FxHashMap<&'source str, Vec<(usize, usize)>>,
    aliases: FxHashSet<&'source str>,
    return_statements: FxHashMap<usize, Vec<Node<'tree>>>,
    returned_literals: FxHashMap<usize, Node<'tree>>,
    members: FxHashMap<(usize, &'source str), usize>,
    unsafe_objects: FxHashSet<usize>,
    generics: FxHashSet<&'source str>,
    object_methods: Vec<MethodHint>,
    needed_objects: FxHashSet<usize>,
    identifier_scopes: FxHashMap<usize, usize>,
    scope_parents: FxHashMap<usize, Option<usize>>,
    function_owners: FxHashMap<usize, Option<usize>>,
    write_scopes: FxHashMap<usize, (usize, usize)>,
    selected_bindings: std::cell::RefCell<FxHashMap<(usize, &'source str), Option<Node<'tree>>>>,
    nominal: bool,
}

impl<'nodes, 'tree, 'source, N: Nav<'tree>> Objects<'nodes, 'tree, 'source, N> {
    pub(super) fn new(
        nodes: &'nodes [Node<'tree>],
        source: &'source str,
        parse: &ParseBlob,
        nav: N,
    ) -> Self {
        Self::with_nominal(nodes, source, parse, true, nav)
    }

    pub(super) fn new_objects_only(
        nodes: &'nodes [Node<'tree>],
        source: &'source str,
        parse: &ParseBlob,
        nav: N,
    ) -> Self {
        Self::with_nominal(nodes, source, parse, false, nav)
    }

    fn with_nominal(
        nodes: &'nodes [Node<'tree>],
        source: &'source str,
        parse: &ParseBlob,
        nominal: bool,
        nav: N,
    ) -> Self {
        let mut profile = crate::stage_timer::StageTimer::new("typescript_index");
        let mut result = Self {
            nodes,
            source,
            nav,
            returns: FxHashMap::default(),
            import_node: None,
            bindings: FxHashMap::default(),
            writes: FxHashMap::default(),
            aliases: FxHashSet::default(),
            return_statements: FxHashMap::default(),
            returned_literals: FxHashMap::default(),
            members: FxHashMap::default(),
            unsafe_objects: FxHashSet::default(),
            generics: FxHashSet::default(),
            object_methods: Vec::new(),
            needed_objects: FxHashSet::default(),
            identifier_scopes: FxHashMap::default(),
            scope_parents: FxHashMap::default(),
            function_owners: FxHashMap::default(),
            write_scopes: FxHashMap::default(),
            selected_bindings: Default::default(),
            nominal,
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
        result.index_nodes();
        profile.finish("scope", format_args!("nodes={}", nodes.len()));
        let mut symbols = HashMap::new();
        let mut members = HashMap::new();
        for symbol in parse
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
        {
            symbols
                .entry((symbol.end_line, symbol.end_col))
                .or_insert(symbol);
            members.insert(
                (symbol.end_line, symbol.end_col, symbol.name.as_str()),
                symbol,
            );
        }
        let mut ambiguous = HashSet::new();
        let mut seen = HashSet::new();
        for node in nodes.iter().filter(|n| FUNCTIONS.contains(&nav.kind(**n))) {
            if let Some((object, name)) = object_member(*node, source, nav) {
                if result.unambiguous_member(object, &name) {
                    if let Some(symbol) = members.get(&(
                        node.end_position().row as u32,
                        node.end_position().column as u32,
                        name.as_str(),
                    )) {
                        let symbol = symbol.scoped_name.clone();
                        result.object_methods.push(MethodHint {
                            owner: object_identity(object),
                            trait_name: None,
                            name,
                            symbol,
                            has_body: field(*node, &["body"]).is_some(),
                            shape: String::new(),
                            private: false,
                        });
                        result.need_object(object);
                    }
                }
            }
            if let Some(object) = result.literal_return(*node) {
                result.returned_literals.insert(node.id(), object);
            }
            if let Some(symbol) = symbols.get(&(
                node.end_position().row as u32,
                node.end_position().column as u32,
            )) {
                if !seen.insert(symbol.scoped_name.clone()) {
                    ambiguous.insert(symbol.scoped_name.clone());
                }
                if let Some(object) = result.returned_object(*node) {
                    if result
                        .returns
                        .insert(symbol.scoped_name.clone(), object_identity(object))
                        .is_some()
                    {
                        ambiguous.insert(symbol.scoped_name.clone());
                    }
                }
            }
        }
        result
            .returns
            .retain(|symbol, _| !ambiguous.contains(symbol));
        let owners = nodes
            .iter()
            .filter(|node| {
                nav.kind(**node) == "object" && result.needed_objects.contains(&node.id())
            })
            .map(|node| object_identity(*node))
            .collect::<HashSet<_>>();
        profile.finish(
            "declarations",
            format_args!(
                "objects={} methods={}",
                result.needed_objects.len(),
                result.object_methods.len()
            ),
        );
        result.returns.retain(|_, ty| owners.contains(ty));
        result
    }

    fn need_object(&mut self, mut object: Node<'tree>) {
        // Keep type records only for literals that supply callables and the
        // literal field containers needed to reach them.
        loop {
            if !self.needed_objects.insert(object.id()) {
                return;
            }
            let nav = self.nav;
            let mut expression = object;
            while let Some(parent) = nav.parent(expression).filter(|p| unwrap(*p) == object) {
                expression = parent;
            }
            let Some(pair) = nav.parent(expression).filter(|p| nav.kind(*p) == "pair") else {
                return;
            };
            let Some(parent) = nav.parent(pair).filter(|p| nav.kind(*p) == "object") else {
                return;
            };
            object = parent;
        }
    }

    pub(super) fn methods(&self) -> &[MethodHint] {
        &self.object_methods
    }

    fn index_binding(&mut self, owner: usize, pattern: Node<'tree>, binding: Node<'tree>) {
        let names = descendants(pattern)
            .into_iter()
            .filter(|node| {
                matches!(
                    node.kind(),
                    "identifier" | "type_identifier" | "shorthand_property_identifier_pattern"
                )
            })
            .map(|node| text(node, self.source))
            .collect::<HashSet<_>>();
        for name in names {
            self.bindings
                .entry((owner, name))
                .and_modify(|current| {
                    if current.is_some_and(|previous| previous != binding) {
                        *current = None;
                    }
                })
                .or_insert(Some(binding));
        }
    }

    // Build source-wide indexes once. Receiver queries only follow lexical
    // ancestors and indexed declarations, never rescan a function's descendants.
    fn index_nodes(&mut self) {
        let mut scopes = Vec::<Node<'tree>>::new();
        let mut functions = Vec::<Node<'tree>>::new();
        let nav = self.nav;
        for &node in self.nodes {
            let kind = nav.kind(node);
            let is_function = FUNCTIONS.contains(&kind);
            let is_scope = matches!(
                kind,
                "program"
                    | "statement_block"
                    | "for_statement"
                    | "for_in_statement"
                    | "catch_clause"
            ) || is_function;
            let contains = |parent: &Node<'tree>| {
                parent.start_byte() <= node.start_byte() && node.end_byte() <= parent.end_byte()
            };
            while scopes.last().is_some_and(|parent| !contains(parent)) {
                scopes.pop();
            }
            while functions.last().is_some_and(|parent| !contains(parent)) {
                functions.pop();
            }
            let parent_scope = scopes.last().copied();
            if is_scope {
                self.scope_parents
                    .insert(node.id(), parent_scope.map(|scope| scope.id()));
                scopes.push(node);
            }
            if is_function {
                functions.push(node);
            }
            if kind == "identifier" {
                if let Some(scope) = scopes.last() {
                    self.identifier_scopes.insert(node.id(), scope.id());
                }
            }
            if self.nominal
                && (is_function
                    || matches!(
                        kind,
                        "variable_declarator"
                            | "required_parameter"
                            | "optional_parameter"
                            | "call_expression"
                            | "jsx_opening_element"
                            | "jsx_self_closing_element"
                    ))
            {
                self.function_owners
                    .insert(node.id(), functions.last().map(|function| function.id()));
            }
            if self.nominal && kind == "type_parameter" {
                if let Some(name) = field(node, &["name"]) {
                    self.generics.insert(text(name, self.source));
                }
            }
            if kind == "import_statement" && self.import_node.is_none() {
                self.import_node = Some(node);
                if let Some(scope) = parent_scope {
                    self.write_scopes
                        .insert(node.id(), (scope.start_byte(), scope.end_byte()));
                }
            }
            if kind == "return_statement" {
                if let Some(function) = functions.last() {
                    self.return_statements
                        .entry(function.id())
                        .or_default()
                        .push(node);
                }
            }
            if kind == "object" {
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "spread_element" {
                        self.unsafe_objects.insert(node.id());
                    }
                    if let Some(key) = field(child, &["key", "name"]) {
                        if key.kind() == "computed_property_name" {
                            self.unsafe_objects.insert(node.id());
                        }
                        let name = text(key, self.source).trim_matches(['\'', '"']);
                        *self.members.entry((node.id(), name)).or_default() += 1;
                    }
                }
            }
            if matches!(
                kind,
                "variable_declarator"
                    | "required_parameter"
                    | "optional_parameter"
                    | "function_declaration"
                    | "class_declaration"
                    | "catch_clause"
                    | "for_in_statement"
                    | "enum_declaration"
                    | "internal_module"
            ) {
                let function_scoped = kind == "for_in_statement"
                    || kind == "variable_declarator"
                        && nav
                            .parent(node)
                            .is_some_and(|parent| nav.kind(parent) == "variable_declaration");
                let owner = if kind == "catch_clause" {
                    Some(node)
                } else if function_scoped {
                    functions
                        .last()
                        .copied()
                        .or_else(|| scopes.first().copied())
                } else {
                    parent_scope
                };
                if let (Some(pattern), Some(owner)) = (
                    field(node, &["name", "pattern", "parameter", "left"]),
                    owner,
                ) {
                    self.write_scopes
                        .insert(node.id(), (owner.start_byte(), owner.end_byte()));
                    self.index_binding(owner.id(), pattern, node);
                    if kind == "for_in_statement" {
                        self.index_binding(node.id(), pattern, node);
                    }
                }
            }
            if is_function {
                if let Some(scope) = parent_scope {
                    self.write_scopes
                        .entry(node.id())
                        .or_insert((scope.start_byte(), scope.end_byte()));
                }
                if let Some(parameter) = field(node, &["parameter"]) {
                    self.index_binding(node.id(), parameter, node);
                }
                if kind == "function_expression" {
                    if let Some(name) = field(node, &["name"]) {
                        self.index_binding(node.id(), name, node);
                    }
                }
            }
            if kind == "variable_declarator" {
                if let Some(value) = field(node, &["value"]).filter(|n| n.kind() == "identifier") {
                    self.aliases.insert(text(value, self.source));
                }
            }
            if matches!(
                kind,
                "assignment_expression"
                    | "augmented_assignment_expression"
                    | "update_expression"
                    | "unary_expression"
            ) {
                if let Some(mut left) = field(node, &["left", "argument"]) {
                    while matches!(left.kind(), "member_expression" | "subscript_expression") {
                        let Some(base) = field(left, &["object"]) else {
                            break;
                        };
                        left = base;
                    }
                    if left.kind() == "identifier" {
                        self.writes
                            .entry(text(left, self.source))
                            .or_default()
                            .push((node.start_byte(), node.end_byte()));
                    }
                }
            }
        }
        for writes in self.writes.values_mut() {
            writes.sort_unstable();
        }
    }

    fn returned_object(&self, function: Node<'tree>) -> Option<Node<'tree>> {
        self.returned_literals.get(&function.id()).copied()
    }

    fn literal_return(&self, function: Node<'tree>) -> Option<Node<'tree>> {
        // An async factory returns a Promise, not the literal itself.
        if matches!(
            function.kind(),
            "generator_function" | "generator_function_declaration"
        ) || function
            .children(&mut function.walk())
            .any(|n| n.kind() == "async")
        {
            return None;
        }
        let body = field(function, &["body"])?;
        let expression = if body.kind() == "statement_block" {
            let returns = self.return_statements.get(&function.id())?;
            if returns.len() != 1 || self.nav.parent(returns[0]) != Some(body) {
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
        !self.unsafe_objects.contains(&object.id())
            && self.members.get(&(object.id(), member)) == Some(&1)
    }

    pub(super) fn extract(&self, facts: &mut DispatchFacts) {
        for &object in self
            .nodes
            .iter()
            .filter(|n| self.nav.kind(**n) == "object" && self.needed_objects.contains(&n.id()))
        {
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
                    .filter(|n| n.kind() == "object" && self.needed_objects.contains(&n.id()))
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
        facts.returns.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    }

    fn binding(&self, identifier: Node<'tree>) -> Option<Node<'tree>> {
        let name = text(identifier, self.source);
        let start = *self.identifier_scopes.get(&identifier.id())?;
        if let Some(selected) = self.selected_bindings.borrow().get(&(start, name)) {
            return *selected;
        }
        let mut current = Some(start);
        let mut selected = None;
        while let Some(scope) = current {
            if let Some(binding) = self.bindings.get(&(scope, name)) {
                selected = Some(binding.unwrap_or(identifier));
                break;
            }
            current = self.scope_parents.get(&scope).copied().flatten();
        }
        self.selected_bindings
            .borrow_mut()
            .insert((start, name), selected);
        selected
    }

    fn written(&self, binding: Node<'tree>, name: &str) -> bool {
        let Some(&(start, end)) = self.write_scopes.get(&binding.id()) else {
            return true;
        };
        if self.aliases.contains(name) {
            return true;
        }
        let Some(writes) = self.writes.get(name) else {
            return false;
        };
        let index = writes.partition_point(|(position, _)| *position < start);
        writes
            .get(index)
            .is_some_and(|(_, position)| *position <= end)
    }

    pub(super) fn receiver(&self, expression: Node<'tree>, call: Node<'tree>) -> Option<String> {
        self.value(expression, call, 0)
            .or_else(|| self.nominal_receiver(expression, call))
    }

    pub(super) fn object_receiver(
        &self,
        expression: Node<'tree>,
        call: Node<'tree>,
    ) -> Option<String> {
        self.value(expression, call, 0)
    }

    fn nominal_receiver(&self, expression: Node<'tree>, call: Node<'tree>) -> Option<String> {
        if !self.nominal {
            return None;
        }
        if expression.kind() != "identifier" {
            return None;
        }
        let binding = self.binding(expression)?;
        if !matches!(
            binding.kind(),
            "variable_declarator" | "required_parameter" | "optional_parameter"
        ) {
            return None;
        }
        if self.function_owners.get(&binding.id()).copied().flatten()
            != self.function_owners.get(&call.id()).copied().flatten()
        {
            return None;
        }
        let annotation = field(binding, &["type"]).or_else(|| {
            binding
                .named_children(&mut binding.walk())
                .find(|n| n.kind() == "type_annotation")
        });
        if let Some(annotation) = annotation {
            let ty = type_name(text(annotation, self.source));
            return (!self.generics.contains(ty.as_str())).then_some(ty);
        }
        if binding.kind() != "variable_declarator"
            || binding.start_byte() >= call.start_byte()
            || self.written(binding, text(expression, self.source))
        {
            return None;
        }
        let initializer = field(binding, &["value"])?;
        if initializer.kind() != "new_expression" {
            return None;
        }
        field(initializer, &["constructor"]).map(|n| type_name(text(n, self.source)))
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
                    || field(binding, &["name"]).is_none_or(|n| n.kind() != "identifier")
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
                        if binding.start_byte() >= call.start_byte()
                            || field(binding, &["name"]).is_none_or(|n| n.kind() != "identifier")
                        {
                            return None;
                        }
                        unwrap(field(binding, &["value"])?)
                    } else {
                        return None;
                    };
                    self.returned_object(function).map(object_identity)
                } else if self.imports.contains(name) {
                    let import = self.import_node?;
                    (!self.written(import, name)).then(|| format!("@return:{name}"))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}
