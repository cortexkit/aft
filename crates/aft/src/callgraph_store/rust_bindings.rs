//! In-memory Rust declaration and lexical-binding indexes. Graph producers
//! reuse their existing return and receiver fact fields; no index is serialized.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use tree_sitter::Node;

#[derive(Clone, Debug)]
pub(super) struct Declaration {
    pub symbol: String,
    pub owner: Option<String>,
    pub return_type: Option<String>,
    pub bytes: Range<usize>,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub(super) enum Initializer {
    Call { callee: String, bytes: Range<usize> },
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
    declarations: HashMap<String, Option<Declaration>>,
    symbols: HashMap<usize, String>,
    calls: HashMap<usize, usize>,
    bindings: HashMap<(usize, String), Vec<Binding>>,
    bounds: HashMap<(usize, String), String>,
    parameters: HashSet<(usize, String)>,
    generic_functions: HashSet<usize>,
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

fn identity(node: Node<'_>, source: &str) -> Option<(String, Option<String>)> {
    let name = text(node.child_by_field_name("name")?, source);
    let mut scope = Vec::new();
    let mut owner = None;
    let mut parent = node.parent();
    let mut implementation = false;
    while let Some(ancestor) = parent {
        if ancestor.kind() == "impl_item" {
            let ty = text(ancestor.child_by_field_name("type")?, source);
            owner = Some(ty.to_string());
            scope.push(
                if let Some(trait_node) = ancestor.child_by_field_name("trait") {
                    format!("{} for {ty}", text(trait_node, source))
                } else {
                    ty.to_string()
                },
            );
            implementation = true;
            break;
        }
        if matches!(ancestor.kind(), "mod_item" | "trait_item") {
            let name = text(ancestor.child_by_field_name("name")?, source).to_string();
            if ancestor.kind() == "trait_item" {
                owner = Some(name.clone());
            }
            scope.push(name);
        }
        parent = ancestor.parent();
    }
    if !implementation {
        scope.reverse();
    }
    scope.push(name.to_string());
    Some((scope.join("::"), owner))
}

impl RustBindings {
    pub fn new(source: &str, root: Node<'_>) -> Self {
        let mut result = Self::default();
        let mut stack = vec![(root, None, root.byte_range())];
        while let Some((node, mut callable, mut scope)) = stack.pop() {
            if matches!(node.kind(), "function_item" | "closure_expression") {
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
            }
            if node.kind() == "block" {
                scope = node.byte_range();
            }
            if matches!(node.kind(), "function_item" | "function_signature_item") {
                if let Some((symbol, owner)) = identity(node, source) {
                    let declaration = Declaration {
                        symbol: symbol.clone(),
                        owner,
                        return_type: node
                            .child_by_field_name("return_type")
                            .map(|n| text(n, source).to_string()),
                        bytes: node.byte_range(),
                        line: node.start_position().row as u32 + 1,
                    };
                    result.symbols.insert(node.start_byte(), symbol.clone());
                    result
                        .declarations
                        .entry(symbol)
                        .and_modify(|old| *old = None)
                        .or_insert(Some(declaration));
                }
            }
            if let Some(callable) = callable {
                if node.kind() == "call_expression" {
                    result.calls.insert(node.start_byte(), callable);
                }
                if node.kind() == "let_declaration" || node.kind() == "parameter" {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let simple = matches!(pattern.kind(), "identifier" | "mut_pattern");
                        let initializer = simple
                            .then(|| node.child_by_field_name("value"))
                            .flatten()
                            .and_then(|value| match value.kind() {
                                "call_expression" => Some(Initializer::Call {
                                    callee: crate::calls::extract_full_callee(&value, source)?,
                                    bytes: value.byte_range(),
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
                } else if !matches!(node.kind(), "function_item" | "closure_expression") {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let mut binding_scope = node;
                        if node.kind() == "let_condition" {
                            while !matches!(
                                binding_scope.kind(),
                                "if_expression" | "while_expression"
                            ) {
                                let Some(parent) = binding_scope.parent() else {
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
            stack.extend(
                node.named_children(&mut node.walk())
                    .map(|child| (child, callable, scope.clone())),
            );
        }
        for bindings in result.bindings.values_mut() {
            bindings.sort_by_key(|b| b.start);
        }
        result
    }

    pub fn declaration(&self, symbol: &str) -> Option<&Declaration> {
        self.declarations.get(symbol)?.as_ref()
    }

    pub fn symbol_at(&self, byte: usize) -> Option<&str> {
        let symbol = self.symbols.get(&byte)?;
        self.declaration(symbol).map(|d| d.symbol.as_str())
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
            .set_language(&crate::parser::grammar_for(crate::parser::LangId::Rust))
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
        let index =
            index("struct Item; impl Item { fn new() -> Self { Self } fn new() -> Self { Self } }");
        assert!(index.declaration("Item::new").is_none());
    }

    #[test]
    fn distinct_trait_implementations_keep_distinct_declarations() {
        let index = index("trait I { fn m(&self); } struct A; struct B; impl I for A { fn m(&self) {} } impl I for B { fn m(&self) {} }");
        assert!(index.declaration("I::m").is_some());
        assert!(index.declaration("I for A::m").is_some());
        assert!(index.declaration("I for B::m").is_some());
    }
}
