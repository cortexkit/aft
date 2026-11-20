use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::super::{field, text, typescript, Nav, FUNCTIONS};
use super::{Declaration, Receiver, Scope, StaticFacts};
use crate::callgraph_store::join::ParseBlob;

const CS_NAMESPACES: &[&str] = &["namespace_declaration", "file_scoped_namespace_declaration"];
const CS_TYPES: &[&str] = &[
    "class_declaration",
    "struct_declaration",
    "record_declaration",
    "record_struct_declaration",
    "interface_declaration",
    "enum_declaration",
];
const JAVA_TYPES: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
];
const TS_TYPES: &[&str] = &["class_declaration", "abstract_class_declaration"];
const TS_NAMESPACES: &[&str] = &["internal_module"];
const TS_SCOPES: &[&str] = &[
    "program",
    "statement_block",
    "for_statement",
    "for_in_statement",
    "catch_clause",
];

fn type_kinds(language: &str) -> &'static [&'static str] {
    match language {
        "csharp" => CS_TYPES,
        "java" => JAVA_TYPES,
        _ => TS_TYPES,
    }
}

pub(in super::super) struct Index<'tree, 'source, N> {
    source: &'source str,
    language: &'source str,
    nav: N,
    facts: StaticFacts,
    types: HashMap<usize, u32>,
    scopes: HashMap<Scope, u32>,
    receivers: HashMap<Receiver, u32>,
    seen_sites: HashSet<u32>,
    values: HashMap<&'source str, Vec<Node<'tree>>>,
    ts_imports: HashMap<&'source str, &'source str>,
    ts_dotted: HashMap<(usize, String), Option<u32>>,
    ts_classes: HashMap<(usize, String), Option<u32>>,
    root_scope: u32,
}

impl<'tree, 'source, N: Nav<'tree>> Index<'tree, 'source, N> {
    pub(in super::super) fn new(
        all: &[Node<'tree>],
        source: &'source str,
        parse: &'source ParseBlob,
        functions: &HashMap<(u32, u32), Node<'tree>>,
        nav: N,
    ) -> Option<Self> {
        let language = parse.language.as_str();
        if !matches!(language, "csharp" | "java") && !typescript::supported(language) {
            return None;
        }
        let mut index = Self {
            source,
            language,
            nav,
            facts: StaticFacts::default(),
            types: HashMap::new(),
            scopes: HashMap::new(),
            receivers: HashMap::new(),
            seen_sites: HashSet::new(),
            values: HashMap::new(),
            ts_imports: HashMap::new(),
            ts_dotted: HashMap::new(),
            ts_classes: HashMap::new(),
            root_scope: 0,
        };
        let mut root = Scope::default();
        let mut usings = HashMap::<Option<usize>, Vec<(Option<String>, String)>>::new();
        if language == "java" {
            if let Some(package) = all.iter().find(|n| n.kind() == "package_declaration") {
                root.namespace = package
                    .named_children(&mut package.walk())
                    .find_map(|n| path(n, source, false))
                    .unwrap_or_default();
            }
            for node in all.iter().filter(|n| n.kind() == "import_declaration") {
                if node
                    .children(&mut node.walk())
                    .any(|n| matches!(n.kind(), "static" | "asterisk"))
                {
                    continue;
                }
                if let Some(target) = node
                    .named_children(&mut node.walk())
                    .find_map(|n| path(n, source, false))
                {
                    let name = target.rsplit('.').next().unwrap_or(&target).to_string();
                    root.aliases.push((name, target));
                }
            }
        } else if language == "csharp" {
            if let Some(file_namespace) = all
                .iter()
                .find(|n| n.kind() == "file_scoped_namespace_declaration")
            {
                root.namespace = field(*file_namespace, &["name"])
                    .and_then(|n| path(n, source, true))
                    .unwrap_or_default();
            }
            for &node in all.iter().filter(|n| n.kind() == "using_directive") {
                let mut cursor = node.walk();
                let children = node.children(&mut cursor).collect::<Vec<_>>();
                let alias = children.iter().position(|n| n.kind() == "=");
                if children.iter().any(|n| n.kind() == "static") {
                    continue;
                }
                let target = children
                    .iter()
                    .skip(alias.map_or(0, |eq| eq + 1))
                    .find_map(|n| path(*n, source, true));
                if let Some(target) = target {
                    let alias = alias.and_then(|eq| eq.checked_sub(1)).map(|i| {
                        text(children[i], source)
                            .trim_start_matches('@')
                            .to_string()
                    });
                    let scope = nav.enclosing(node, CS_NAMESPACES).map(|n| n.id());
                    usings.entry(scope).or_default().push((alias, target));
                }
            }
            add_usings(&mut root, usings.get(&None));
        } else {
            index.ts_imports = parse
                .imports
                .iter()
                .filter(|i| !i.type_only)
                .flat_map(|import| {
                    import
                        .default_import
                        .iter()
                        .map(String::as_str)
                        .chain(import.names.iter().filter_map(|name| {
                            (!name.starts_with("type "))
                                .then(|| name.split_whitespace().last())
                                .flatten()
                        }))
                        .map(|name| (name, import.module_path.as_str()))
                })
                .collect();
        }
        index.root_scope = index.scope(root.clone());
        // The walk visits an enclosing type before its children.
        for &node in all
            .iter()
            .filter(|n| type_kinds(language).contains(&n.kind()))
        {
            if language == "java" && nav.enclosing(node, FUNCTIONS).is_some() {
                continue;
            }
            let Some(name) = field(node, &["name"]) else {
                continue;
            };
            let name = declared_name(node, text(name, source), language == "csharp");
            let outer = nav
                .parent(node)
                .and_then(|n| nav.enclosing(n, type_kinds(language)))
                .and_then(|n| index.types.get(&n.id()))
                .copied();
            let mut scope = root.clone();
            if language == "csharp" {
                let mut namespaces = Vec::new();
                let mut current = nav.enclosing(node, CS_NAMESPACES);
                while let Some(ns) = current {
                    namespaces.push(ns);
                    current = nav
                        .parent(ns)
                        .and_then(|parent| nav.enclosing(parent, CS_NAMESPACES));
                }
                for ns in namespaces.into_iter().rev() {
                    if ns.kind() != "file_scoped_namespace_declaration" {
                        if let Some(name) = field(ns, &["name"]).and_then(|n| path(n, source, true))
                        {
                            scope.namespace = join(&scope.namespace, &name);
                        }
                    }
                    add_usings(&mut scope, usings.get(&Some(ns.id())));
                }
            }
            let scope_id = index.scope(scope.clone());
            let prefix = outer.map_or(scope.namespace.as_str(), |i| {
                index.facts.types[i as usize].name.as_str()
            });
            let mut qualified = join(prefix, &name);
            let id = index.facts.types.len() as u32;
            if typescript::supported(language) {
                if let Some(scope) = ts_lexical_scope(node, nav) {
                    index
                        .ts_classes
                        .entry((scope, name.clone()))
                        .and_modify(|old| *old = None)
                        .or_insert(Some(id));
                }
                if let Some((scope, prefix)) = ts_namespace(node, source, nav) {
                    qualified = join(&prefix, &name);
                    index
                        .ts_dotted
                        .entry((scope, qualified.clone()))
                        .and_modify(|old| *old = None)
                        .or_insert(Some(id));
                }
            }
            let declaration = Declaration {
                name: qualified,
                scope: scope_id,
                bases: bases(node, source, language),
                values: Vec::new(),
                methods: Vec::new(),
            };
            index.types.insert(node.id(), id);
            index.facts.types.push(declaration);
        }
        if matches!(language, "csharp" | "java") {
            index.index_values(all);
        }
        for symbol in parse
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "method" | "function"))
        {
            let Some(&node) = functions.get(&(symbol.end_line, symbol.end_col)) else {
                continue;
            };
            if !node.children(&mut node.walk()).any(|child| {
                child.kind() == "static"
                    || (matches!(child.kind(), "modifier" | "modifiers")
                        && child
                            .children(&mut child.walk())
                            .any(|n| n.kind() == "static"))
                    || child.kind() == "modifier" && text(child, source) == "static"
            }) {
                continue;
            }
            let Some(parent) = nav.parent(node) else {
                continue;
            };
            let Some(owner) = nav.parent(parent).and_then(|n| index.types.get(&n.id())) else {
                continue;
            };
            index.facts.types[*owner as usize]
                .methods
                .push((symbol.name.clone(), symbol.scoped_name.clone()));
        }
        Some(index)
    }

    fn scope(&mut self, scope: Scope) -> u32 {
        let id = self.facts.scopes.len() as u32;
        *self.scopes.entry(scope.clone()).or_insert_with(|| {
            self.facts.scopes.push(scope);
            id
        })
    }

    fn index_values(&mut self, all: &[Node<'tree>]) {
        let nav = self.nav;
        for &node in all {
            let kind = node.kind();
            let name = match kind {
                "variable_declarator"
                | "parameter"
                | "formal_parameter"
                | "spread_parameter"
                | "catch_formal_parameter"
                | "property_declaration"
                | "event_declaration"
                | "enum_constant"
                | "enum_member_declaration"
                | "declaration_expression"
                | "declaration_pattern"
                | "recursive_pattern"
                | "var_pattern"
                | "from_clause"
                | "let_clause"
                | "join_clause"
                | "query_continuation"
                | "enhanced_for_statement"
                | "catch_declaration"
                | "for_each_statement" => field(node, &["name", "left"]),
                "single_variable_designation" => node.named_child(0).or(Some(node)),
                "lambda_expression" => field(node, &["parameter", "parameters"]),
                _ => continue,
            };
            let Some(name) = name else { continue };
            let type_owner = nav.enclosing(node, type_kinds(self.language));
            let function = nav.enclosing(node, FUNCTIONS);
            let names = binding_names(name);
            for binding in names {
                let name = text(binding, self.source).trim_start_matches('@');
                if let Some(function) = function
                    .filter(|f| type_owner.is_none_or(|ty| f.start_byte() >= ty.start_byte()))
                {
                    self.values.entry(name).or_default().push(function);
                } else if let Some(owner) = type_owner.and_then(|n| self.types.get(&n.id())) {
                    self.facts.types[*owner as usize]
                        .values
                        .push(name.to_string());
                }
            }
        }
    }

    pub(in super::super) fn site(
        &mut self,
        ordinal: u32,
        written: Option<Node<'tree>>,
        call: Node<'tree>,
        ts_binding: impl FnOnce(Node<'tree>) -> (Option<Node<'tree>>, Option<usize>),
    ) {
        // Several call references can name one AST ordinal. Dispatch
        // keeps the first hint at that ordinal, including a hint without
        // static evidence. Keep the evidence from the same occurrence.
        if !self.seen_sites.insert(ordinal) {
            return;
        }
        let Some(written) = written else { return };
        let Some(name) = path(written, self.source, self.language == "csharp") else {
            return;
        };
        let root = name
            .trim_start_matches("global::")
            .split(['.', '`'])
            .next()
            .unwrap_or(&name);
        if matches!(root, "this" | "super" | "base") {
            return;
        }
        let owner = self
            .nav
            .enclosing(call, type_kinds(self.language))
            .and_then(|n| self.types.get(&n.id()).copied());
        let scope = owner.map_or(self.root_scope, |i| self.facts.types[i as usize].scope);
        let owner = match self.language {
            "java" | "csharp" => {
                if !name.starts_with("global::")
                    && (self.values.get(root).is_some_and(|scopes| {
                        scopes.iter().any(|scope| {
                            scope.start_byte() <= call.start_byte()
                                && call.end_byte() <= scope.end_byte()
                        })
                    }) || enclosing_value(&self.facts.types, owner, root))
                {
                    return;
                }
                if self.language == "csharp" && owner.is_none() {
                    return;
                }
                owner
            }
            _ => {
                // A local declaration must be the class in the receiver's lexical scope,
                // not an unrelated class elsewhere in the file with the same spelling.
                let Some(identifier) = root_identifier(written) else {
                    return;
                };
                let (binding, binding_scope) = ts_binding(identifier);
                if name.contains('.') {
                    let Some(id) = binding_scope.and_then(|scope| {
                        self.ts_dotted
                            .get(&(scope, name.clone()))
                            .copied()
                            .flatten()
                    }) else {
                        return;
                    };
                    Some(id)
                } else if let Some(binding) = binding {
                    let merged = (binding == identifier)
                        .then(|| {
                            binding_scope.and_then(|scope| {
                                self.ts_dotted
                                    .keys()
                                    .any(|(id, path)| {
                                        *id == scope
                                            && path
                                                .strip_prefix(&name)
                                                .is_some_and(|tail| tail.starts_with('.'))
                                    })
                                    .then(|| {
                                        self.ts_classes
                                            .get(&(scope, name.clone()))
                                            .copied()
                                            .flatten()
                                    })
                                    .flatten()
                            })
                        })
                        .flatten();
                    let Some(id) = self.types.get(&binding.id()).copied().or(merged) else {
                        return;
                    };
                    Some(id)
                } else if self.ts_imports.contains_key(name.as_str()) {
                    None
                } else {
                    return;
                }
            }
        };
        let module = owner
            .is_none()
            .then(|| self.ts_imports.get(name.as_str()).map(|m| (*m).to_string()))
            .flatten();
        let receiver = Receiver {
            name,
            scope,
            owner,
            module,
        };
        let id = self.facts.receivers.len() as u32;
        let id = *self.receivers.entry(receiver.clone()).or_insert_with(|| {
            self.facts.receivers.push(receiver);
            id
        });
        self.facts.sites.push((ordinal, id));
    }

    pub(in super::super) fn finish(mut self) -> Option<Box<StaticFacts>> {
        self.facts.sites.sort_unstable();
        for declaration in &mut self.facts.types {
            declaration.values.sort_unstable();
            declaration.values.dedup();
            declaration.methods.sort_unstable();
            declaration.methods.dedup();
        }
        (!self.facts.is_empty()).then(|| Box::new(self.facts))
    }
}

fn enclosing_value(types: &[Declaration], owner: Option<u32>, name: &str) -> bool {
    let Some(owner) = owner.map(|i| &types[i as usize]) else {
        return false;
    };
    types.iter().any(|ty| {
        ty.values.iter().any(|value| value == name)
            && (ty.name == owner.name
                || owner
                    .name
                    .strip_prefix(&ty.name)
                    .is_some_and(|suffix| suffix.starts_with('.')))
    })
}

fn ts_namespace<'tree>(
    mut node: Node<'tree>,
    source: &str,
    nav: impl Nav<'tree>,
) -> Option<(usize, String)> {
    let mut parents = Vec::new();
    loop {
        let Some(namespace) = nav
            .parent(node)
            .and_then(|parent| nav.enclosing(parent, TS_NAMESPACES))
        else {
            break;
        };
        // A class in a namespace can only be named from outside that body if
        // it is exported. The same holds for intermediate namespaces.
        if !nav
            .parent(node)
            .is_some_and(|parent| parent.kind() == "export_statement")
        {
            return None;
        }
        parents.push(path(field(namespace, &["name"])?, source, false)?);
        node = namespace;
    }
    if parents.is_empty() {
        return None;
    }
    parents.reverse();
    Some((ts_lexical_scope(node, nav)?, parents.join(".")))
}

fn ts_lexical_scope<'tree>(node: Node<'tree>, nav: impl Nav<'tree>) -> Option<usize> {
    let mut parent = nav.parent(node)?;
    while !TS_SCOPES.contains(&parent.kind()) && !FUNCTIONS.contains(&parent.kind()) {
        parent = nav.parent(parent)?;
    }
    Some(parent.id())
}

fn binding_names(node: Node<'_>) -> Vec<Node<'_>> {
    if matches!(node.kind(), "identifier" | "single_variable_designation") {
        return vec![node];
    }
    let mut names = Vec::new();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier" {
            names.push(node);
        } else {
            pending.extend(node.named_children(&mut node.walk()));
        }
    }
    names
}

fn add_usings(scope: &mut Scope, entries: Option<&Vec<(Option<String>, String)>>) {
    for (alias, target) in entries.into_iter().flatten() {
        if let Some(alias) = alias {
            scope.aliases.retain(|(name, _)| name != alias);
            scope.aliases.push((alias.clone(), target.clone()));
        } else {
            scope.imports.push(target.clone());
        }
    }
}

pub(super) fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn declared_name(node: Node<'_>, name: &str, arity: bool) -> String {
    let name = name.trim_start_matches('@');
    if arity {
        if let Some(params) = node
            .named_children(&mut node.walk())
            .find(|n| n.kind() == "type_parameter_list")
        {
            return format!(
                "{name}`{}",
                params
                    .named_children(&mut params.walk())
                    .filter(|n| n.kind() == "type_parameter")
                    .count()
            );
        }
    }
    name.to_string()
}

fn root_identifier(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(node),
        "generic_name" | "generic_type" | "instantiation_expression" => {
            root_identifier(node.named_child(0)?)
        }
        "member_access_expression" | "member_expression" => {
            root_identifier(field(node, &["expression", "object"])?)
        }
        _ => None,
    }
}

fn path(node: Node<'_>, source: &str, arity: bool) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" | "property_identifier" => {
            Some(text(node, source).trim_start_matches('@').to_string())
        }
        "generic_name" | "generic_type" | "instantiation_expression" => {
            let name = path(node.named_child(0)?, source, arity)?;
            if !arity {
                return Some(name);
            }
            let args = node
                .named_children(&mut node.walk())
                .find(|n| n.kind() == "type_argument_list")?;
            let commas = args
                .children(&mut args.walk())
                .filter(|n| n.kind() == ",")
                .count();
            Some(format!("{name}`{}", commas + 1))
        }
        "field_access"
        | "member_expression"
        | "member_access_expression"
        | "qualified_name"
        | "scoped_identifier"
        | "scoped_type_identifier"
        | "nested_type_identifier"
        | "nested_identifier"
        | "alias_qualified_name" => {
            let mut cursor = node.walk();
            let mut children = node
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let first = path(children.next()?, source, arity)?;
            let second = path(children.next()?, source, arity)?;
            if children.next().is_some() {
                return None;
            }
            if node.kind() == "alias_qualified_name" {
                (first == "global").then(|| format!("global::{second}"))
            } else {
                Some(join(&first, &second))
            }
        }
        _ => None,
    }
}

fn bases(node: Node<'_>, source: &str, language: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut children = node
        .named_children(&mut node.walk())
        .filter(|n| matches!(n.kind(), "base_list" | "superclass" | "class_heritage"))
        .collect::<Vec<_>>();
    while let Some(child) = children.pop() {
        if let Some(base) = path(child, source, language == "csharp") {
            out.push(base);
        } else if matches!(
            child.kind(),
            "base_list"
                | "superclass"
                | "class_heritage"
                | "extends_clause"
                | "primary_constructor_base_type"
        ) {
            children.extend(child.named_children(&mut child.walk()).filter(|n| {
                !n.is_extra()
                    && !matches!(
                        n.kind(),
                        "implements_clause" | "type_arguments" | "argument_list"
                    )
            }));
        } else if language == "csharp" {
            // A base we cannot read is not evidence of a closed hierarchy.
            out.push(text(child, source).to_string());
        }
    }
    out
}
