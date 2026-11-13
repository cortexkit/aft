//! In-memory Rust declaration and lexical-binding indexes. Graph producers
//! reuse their existing return and receiver fact fields; no index is serialized.

// Every map here is looked up, never iterated in an order that shows in
// the output, so a fast hash serves them.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::ops::Range;
use tree_sitter::Node;

use super::join::syntax_index::Nav;

#[derive(Clone, Debug)]
pub(super) struct Declaration {
    pub symbol: String,
    pub owner: Option<String>,
    pub return_type: Option<String>,
    pub bytes: Range<usize>,
}

pub(super) fn has_self_parameter(node: Node<'_>, source: &str) -> bool {
    node.child_by_field_name("parameters")
        .is_some_and(|parameters| {
            parameters
                .named_children(&mut parameters.walk())
                .any(|parameter| {
                    parameter.kind() == "self_parameter"
                        || parameter
                            .child_by_field_name("pattern")
                            .is_some_and(|pattern| {
                                text(pattern, source).trim_start_matches("mut ") == "self"
                            })
                })
        })
}

#[derive(Clone, Debug)]
pub(super) enum Initializer {
    Call { callee: String },
    Nominal(String),
}

#[derive(Clone, Debug)]
pub(super) struct Binding {
    start: usize,
    scope: Range<usize>,
    pub annotation: Option<String>,
    pub initializer: Option<Initializer>,
}

#[derive(Default)]
pub(super) struct RustBindings {
    declarations: HashMap<String, Option<usize>>,
    symbols: HashMap<usize, usize>,
    records: Vec<Declaration>,
    calls: HashMap<usize, usize>,
    bindings: HashMap<(usize, String), Vec<Binding>>,
    bounds: HashMap<(usize, String), String>,
    parameters: HashSet<(usize, String)>,
    generic_functions: HashSet<usize>,
    unit_structs: HashMap<String, bool>,
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn names(pattern: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" {
            names.push(text(node, source).to_string());
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    names
}

fn identity<'tree>(
    node: Node<'tree>,
    source: &str,
    types: &HashSet<String>,
    nav: impl Nav<'tree>,
) -> Option<(String, Option<String>)> {
    let name = text(node.child_by_field_name("name")?, source);
    let mut scope = Vec::new();
    let mut owner = None;
    let mut parent = nav.parent(node);
    let mut implementation = false;
    let mut qualify = false;
    while let Some(ancestor) = parent {
        let ancestor_kind = nav.kind(ancestor);
        if ancestor_kind == "impl_item" && !implementation {
            let ty = text(ancestor.child_by_field_name("type")?, source);
            let base = ty.split('<').next().unwrap_or(ty);
            let local = types.contains(&crate::callgraph::rust_module_qualified_name(
                ancestor, source, base, nav,
            ));
            qualify = local
                || ancestor
                    .child_by_field_name("trait")
                    .is_some_and(|trait_node| {
                        let name = text(trait_node, source)
                            .split('<')
                            .next()
                            .unwrap_or_default();
                        types.contains(&crate::callgraph::rust_module_qualified_name(
                            ancestor, source, name, nav,
                        ))
                    });
            let generic =
                ancestor
                    .child_by_field_name("type_parameters")
                    .is_some_and(|parameters| {
                        parameters
                            .named_children(&mut parameters.walk())
                            .any(|parameter| {
                                parameter
                                    .child_by_field_name("name")
                                    .is_some_and(|name| text(name, source) == ty)
                            })
                    });
            owner = (local && !generic)
                .then(|| crate::callgraph::rust_module_qualified_name(ancestor, source, ty, nav));
            scope.push(
                if let Some(trait_node) = ancestor.child_by_field_name("trait") {
                    format!("{} for {ty}", text(trait_node, source))
                } else {
                    ty.to_string()
                },
            );
            implementation = true;
            parent = nav.parent(ancestor);
            continue;
        }
        if ancestor_kind == "mod_item" && (!implementation || qualify)
            || ancestor_kind == "trait_item" && !implementation
        {
            let name = text(ancestor.child_by_field_name("name")?, source).to_string();
            if ancestor_kind == "trait_item" {
                owner = Some(crate::callgraph::rust_module_qualified_name(
                    ancestor, source, &name, nav,
                ));
            }
            scope.push(name);
        }
        parent = nav.parent(ancestor);
    }
    scope.reverse();
    scope.push(name.to_string());
    Some((scope.join("::"), owner))
}

impl RustBindings {
    #[cfg(test)]
    pub fn new(source: &str, root: Node<'_>) -> Self {
        Self::new_with(source, root, super::join::syntax_index::Direct)
    }

    #[cfg(test)]
    pub fn unit_struct(&self, symbol: &str) -> bool {
        self.unit_structs.get(symbol).copied() == Some(true)
    }

    /// [`RustBindings::new`], walking and looking up with `nav`.
    pub fn new_with<'tree>(source: &str, root: Node<'tree>, nav: impl Nav<'tree>) -> Self {
        let mut result = Self::default();
        let mut types = HashSet::default();
        nav.walk_named(root, |node, kind| {
            if matches!(
                kind,
                "struct_item" | "enum_item" | "trait_item" | "type_item"
            ) {
                if let Some(name) = node.child_by_field_name("name") {
                    types.insert(crate::callgraph::rust_module_qualified_name(
                        node,
                        source,
                        text(name, source),
                        nav,
                    ));
                }
            }
            true
        });
        let mut stack = vec![(root, None, root.byte_range())];
        while let Some((node, mut callable, mut scope)) = stack.pop() {
            let kind = nav.kind(node);
            if kind == "struct_item" {
                if let Some(name) = node.child_by_field_name("name") {
                    let mut parts = vec![text(name, source).to_string()];
                    let mut parent = nav.parent(node);
                    while let Some(ancestor) = parent {
                        if nav.kind(ancestor) == "mod_item" {
                            if let Some(name) = ancestor.child_by_field_name("name") {
                                parts.push(text(name, source).to_string());
                            }
                        }
                        parent = nav.parent(ancestor);
                    }
                    parts.reverse();
                    result
                        .unit_structs
                        .entry(parts.join("::"))
                        .and_modify(|unit| *unit = false)
                        .or_insert(node.child_by_field_name("body").is_none());
                }
            }
            if matches!(kind, "function_item" | "closure_expression") {
                callable = Some(node.start_byte());
                scope = node.byte_range();
                if let Some(parameters) = node.child_by_field_name("parameters") {
                    for name in names(parameters, source) {
                        result
                            .bindings
                            .entry((node.start_byte(), name))
                            .or_default()
                            .push(Binding {
                                start: parameters.start_byte(),
                                scope: scope.clone(),
                                annotation: None,
                                initializer: None,
                            });
                    }
                }
                if let Some(parameters) = node.child_by_field_name("type_parameters") {
                    result.generic_functions.insert(node.start_byte());
                    for parameter in parameters.named_children(&mut parameters.walk()) {
                        if let Some(name) = parameter.child_by_field_name("name") {
                            result
                                .parameters
                                .insert((node.start_byte(), text(name, source).to_string()));
                        }
                        if let (Some(name), Some(bounds)) = (
                            parameter.child_by_field_name("name"),
                            parameter.child_by_field_name("bounds"),
                        ) {
                            result.bounds.insert(
                                (node.start_byte(), text(name, source).to_string()),
                                text(bounds, source).to_string(),
                            );
                        }
                    }
                }
                let mut parent = nav.parent(node);
                while let Some(ancestor) = parent {
                    if nav.kind(ancestor) == "impl_item" {
                        if let Some(parameters) = ancestor.child_by_field_name("type_parameters") {
                            result.generic_functions.insert(node.start_byte());
                            for parameter in parameters.named_children(&mut parameters.walk()) {
                                if let Some(name) = parameter.child_by_field_name("name") {
                                    let key = (node.start_byte(), text(name, source).to_string());
                                    if result.parameters.insert(key.clone()) {
                                        if let Some(bounds) =
                                            parameter.child_by_field_name("bounds")
                                        {
                                            result
                                                .bounds
                                                .insert(key, text(bounds, source).to_string());
                                        }
                                    }
                                }
                            }
                        }
                        break;
                    }
                    parent = nav.parent(ancestor);
                }
            }
            if kind == "block" {
                scope = node.byte_range();
            }
            if matches!(kind, "function_item" | "function_signature_item") {
                if let Some((symbol, owner)) = identity(node, source, &types, nav) {
                    let declaration = Declaration {
                        symbol: symbol.clone(),
                        owner,
                        return_type: node
                            .child_by_field_name("return_type")
                            .map(|n| text(n, source).to_string()),
                        bytes: node.byte_range(),
                    };
                    let position = result.records.len();
                    result.symbols.insert(node.start_byte(), position);
                    result
                        .declarations
                        .entry(symbol)
                        .and_modify(|old| *old = None)
                        .or_insert(Some(position));
                    result.records.push(declaration);
                }
            }
            if let Some(callable) = callable {
                if kind == "call_expression" {
                    result.calls.insert(node.start_byte(), callable);
                }
                if kind == "let_declaration" || kind == "parameter" {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let simple = matches!(pattern.kind(), "identifier" | "mut_pattern");
                        let initializer = simple
                            .then(|| node.child_by_field_name("value"))
                            .flatten()
                            .and_then(|value| match value.kind() {
                                "call_expression" => Some(Initializer::Call {
                                    callee: crate::calls::extract_full_callee(&value, source)?,
                                }),
                                "struct_expression" => Some(Initializer::Nominal(
                                    text(value.child_by_field_name("name")?, source).to_string(),
                                )),
                                "identifier"
                                    if text(value, source)
                                        .chars()
                                        .next()
                                        .is_some_and(char::is_uppercase) =>
                                {
                                    Some(Initializer::Nominal(text(value, source).to_string()))
                                }
                                _ => None,
                            });
                        let binding = Binding {
                            start: node.start_byte(),
                            scope: scope.clone(),
                            annotation: node
                                .child_by_field_name("type")
                                .map(|n| text(n, source).to_string()),
                            initializer,
                        };
                        for name in names(pattern, source) {
                            result
                                .bindings
                                .entry((callable, name))
                                .or_default()
                                .push(binding.clone());
                        }
                    }
                } else if !matches!(kind, "function_item" | "closure_expression") {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let mut binding_scope = node;
                        if kind == "let_condition" {
                            while !matches!(
                                nav.kind(binding_scope),
                                "if_expression" | "while_expression"
                            ) {
                                let Some(parent) = nav.parent(binding_scope) else {
                                    break;
                                };
                                binding_scope = parent;
                            }
                        }
                        for name in names(pattern, source) {
                            result
                                .bindings
                                .entry((callable, name))
                                .or_default()
                                .push(Binding {
                                    start: pattern.start_byte(),
                                    scope: binding_scope.byte_range(),
                                    annotation: None,
                                    initializer: None,
                                });
                        }
                    }
                }
            }
            nav.for_each_named_child(node, |child| stack.push((child, callable, scope.clone())));
        }
        for bindings in result.bindings.values_mut() {
            bindings.sort_by_key(|b| b.start);
        }
        result
    }

    pub fn declaration(&self, symbol: &str) -> Option<&Declaration> {
        self.records.get((*self.declarations.get(symbol)?)?)
    }

    /// Extraction retains every positional declaration, even when duplicate
    /// names cannot supply a unique binding for a call.
    pub fn declaration_at(&self, byte: usize) -> Option<&Declaration> {
        self.records.get(*self.symbols.get(&byte)?)
    }

    pub fn symbol_at(&self, byte: usize) -> Option<&str> {
        let declaration = self.declaration_at(byte)?;
        self.declaration(&declaration.symbol)
            .map(|d| d.symbol.as_str())
    }

    pub fn binding(&self, name: &str, call_byte: usize) -> Option<&Binding> {
        let callable = self.calls.get(&call_byte)?;
        let bindings = self.bindings.get(&(*callable, name.to_string()))?;
        let end = bindings.partition_point(|b| b.start < call_byte);
        bindings[..end]
            .iter()
            .rev()
            .find(|b| b.scope.contains(&call_byte))
    }

    pub fn bound(&self, name: &str, call_byte: usize) -> Option<&str> {
        self.bounds
            .get(&(*self.calls.get(&call_byte)?, name.to_string()))
            .map(String::as_str)
    }

    pub fn type_parameter(&self, name: &str, call_byte: usize) -> bool {
        self.calls
            .get(&call_byte)
            .is_some_and(|callable| self.parameters.contains(&(*callable, name.to_string())))
    }

    pub fn generic_callable(&self, call_byte: usize) -> bool {
        self.calls
            .get(&call_byte)
            .is_some_and(|callable| self.generic_functions.contains(callable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(source: &str) -> RustBindings {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::parser::grammar_for(crate::parser::LangId::Rust).unwrap())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        RustBindings::new(source, tree.root_node())
    }

    #[test]
    fn local_bindings_are_indexed_by_callable_and_lexical_scope() {
        let source = "fn run(value: &Item) { let value = Item::new(); value.m(); { let value = opaque(); value.m(); } value.m(); let closure = |value| value.m(); }";
        let index = index(source);
        let calls = source
            .match_indices("value.m()")
            .map(|(byte, _)| byte)
            .collect::<Vec<_>>();
        let callee = |byte| match index.binding("value", byte).unwrap().initializer.as_ref() {
            Some(Initializer::Call { callee, .. }) => Some(callee.as_str()),
            _ => None,
        };
        assert_eq!(callee(calls[0]), Some("Item::new"));
        assert_eq!(callee(calls[1]), Some("opaque"));
        assert_eq!(callee(calls[2]), Some("Item::new"));
        assert_eq!(callee(calls[3]), None);
    }

    #[test]
    fn return_declarations_use_owner_identity_not_name_sized_ranges() {
        let source = "struct Item; impl Item { /// Factory.\n fn new() -> Self { Self } } trait Analyzer { fn get(&self) -> &dyn Analyzer { todo!() } }";
        let index = index(source);
        assert_eq!(
            index
                .declaration("Item::new")
                .unwrap()
                .return_type
                .as_deref(),
            Some("Self")
        );
        assert_eq!(
            index
                .declaration("Analyzer::get")
                .unwrap()
                .return_type
                .as_deref(),
            Some("&dyn Analyzer")
        );
        assert!(index.declaration("new").is_none());
    }

    #[test]
    fn duplicate_owner_declarations_are_not_guessed() {
        let source =
            "struct Item; impl Item { fn new() -> Self { Self } fn new() -> Self { Self } }";
        let index = index(source);
        assert!(index.declaration("Item::new").is_none());
        let positions = source
            .match_indices("fn new")
            .map(|(byte, _)| byte)
            .collect::<Vec<_>>();
        assert_eq!(positions.len(), 2);
        for position in positions {
            let declaration = index.declaration_at(position).unwrap();
            assert_eq!(declaration.symbol, "Item::new");
            assert_eq!(declaration.owner.as_deref(), Some("Item"));
            assert_eq!(declaration.bytes.start, position);
            assert!(index.symbol_at(position).is_none());
        }
    }

    #[test]
    fn distinct_trait_implementations_keep_distinct_declarations() {
        let index = index("trait I { fn m(&self); } struct A; struct B; impl I for A { fn m(&self) {} } impl I for B { fn m(&self) {} }");
        assert!(index.declaration("I::m").is_some());
        assert!(index.declaration("I for A::m").is_some());
        assert!(index.declaration("I for B::m").is_some());
    }

    #[test]
    fn bare_values_require_a_unique_unit_struct_declaration() {
        let index = index(
            "struct Unit; struct Named { field: u8 } struct Tuple(u8); mod inner { struct Unit; }",
        );
        assert!(index.unit_struct("Unit"));
        assert!(index.unit_struct("inner::Unit"));
        assert!(!index.unit_struct("Named"));
        assert!(!index.unit_struct("Tuple"));
    }

    #[test]
    fn local_inline_owners_are_qualified_without_moving_imported_types() {
        let index = index("struct Root; mod local { struct Item; impl Item { fn new() -> Self { Self } } } mod implementation { use super::Root; impl Root { fn new() -> Self { Root } } }");
        assert_eq!(
            index
                .declaration("local::Item::new")
                .unwrap()
                .owner
                .as_deref(),
            Some("local::Item")
        );
        assert_eq!(
            index.declaration("Root::new").unwrap().owner.as_deref(),
            None
        );
    }
}
