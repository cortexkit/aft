//! Python imports use lexical bindings and dotted module paths, not JS specifiers.

use crate::imports::{self, ImportBlock, ImportForm, ImportStatement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tree_sitter::Node;

/// A selected import for one call. An absent target means a local binding
/// shadows an import, so resolution must not fall back to a same-named function.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CallBinding {
    pub byte_start: usize,
    pub byte_end: usize,
    pub target: Option<(String, String)>,
}

#[derive(Clone, PartialEq, Eq)]
enum Binding {
    Imported {
        module: String,
        symbol: Option<String>,
        prefix: String,
    },
    LocalDefinition,
    Shadowed,
}

struct Scope {
    parent: Option<usize>,
    class: bool,
    names: HashMap<String, Binding>,
}

struct Extractor<'a> {
    source: &'a str,
    scopes: Vec<Scope>,
    imports: Vec<ImportStatement>,
    calls: Vec<(Node<'a>, usize)>,
}

pub(super) fn extract(source: &str, root: Node<'_>) -> (ImportBlock, Vec<CallBinding>) {
    let mut extractor = Extractor {
        source,
        scopes: vec![Scope {
            parent: None,
            class: false,
            names: HashMap::new(),
        }],
        imports: Vec::new(),
        calls: Vec::new(),
    };
    extractor.visit(root, 0);
    let bindings = extractor
        .calls
        .iter()
        .filter_map(|(call, scope)| {
            let callee = call.child_by_field_name("function")?;
            let parts = source[callee.byte_range()]
                .split('.')
                .map(str::trim)
                .collect::<Vec<_>>();
            if parts.iter().any(|part| {
                part.is_empty() || !part.chars().all(|c| c == '_' || c.is_alphanumeric())
            }) {
                return None;
            }
            let binding = extractor.lookup(*scope, parts[0])?;
            if *binding == Binding::LocalDefinition {
                return None;
            }
            let target = match binding {
                Binding::Imported {
                    module,
                    symbol: Some(symbol),
                    ..
                } if parts.len() == 1 => Some((module.clone(), symbol.clone())),
                Binding::Imported {
                    module,
                    symbol: None,
                    prefix,
                } => {
                    let prefix = prefix.split('.').collect::<Vec<_>>();
                    (parts.len() == prefix.len() + 1 && parts.starts_with(&prefix))
                        .then(|| (module.clone(), parts.last().unwrap().to_string()))
                }
                _ => None,
            };
            Some(CallBinding {
                byte_start: call.start_byte(),
                byte_end: call.end_byte(),
                target,
            })
        })
        .collect();
    (
        ImportBlock {
            imports: extractor.imports,
            byte_range: None,
        },
        bindings,
    )
}

impl<'a> Extractor<'a> {
    fn bind(&mut self, scope: usize, name: &str, binding: Binding) {
        self.scopes[scope]
            .names
            .entry(name.to_string())
            .and_modify(|previous| {
                if *previous != binding {
                    *previous = Binding::Shadowed;
                }
            })
            .or_insert(binding);
    }

    fn lookup(&self, scope: usize, name: &str) -> Option<&Binding> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let frame = &self.scopes[id];
            // Class attributes are not lexical variables inside methods.
            if id == scope || !frame.class {
                if let Some(binding) = frame.names.get(name) {
                    return Some(binding);
                }
            }
            current = frame.parent;
        }
        None
    }

    fn visit(&mut self, node: Node<'a>, mut scope: usize) {
        if matches!(
            node.kind(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                self.bind(
                    scope,
                    &self.source[name.byte_range()],
                    Binding::LocalDefinition,
                );
            }
            let id = self.scopes.len();
            self.scopes.push(Scope {
                parent: Some(scope),
                class: node.kind() == "class_definition",
                names: HashMap::new(),
            });
            scope = id;
            if let Some(parameters) = node.child_by_field_name("parameters") {
                for parameter in parameters.named_children(&mut parameters.walk()) {
                    let name = if parameter.kind() == "identifier" {
                        Some(parameter)
                    } else {
                        parameter
                            .child_by_field_name("name")
                            .or_else(|| parameter.named_child(0))
                    };
                    if let Some(name) = name.filter(|n| n.kind() == "identifier") {
                        self.bind(scope, &self.source[name.byte_range()], Binding::Shadowed);
                    }
                }
            }
        }
        if let Some(import) = imports::parse_py_import_node(self.source, &node) {
            self.import(scope, import);
            return;
        }
        if matches!(
            node.kind(),
            "assignment" | "augmented_assignment" | "for_statement" | "named_expression"
        ) {
            if let Some(left) = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("name"))
            {
                self.shadow_pattern(left, scope);
            }
        }
        if node.kind() == "call" {
            self.calls.push((node, scope));
        }
        for child in node.named_children(&mut node.walk()) {
            self.visit(child, scope);
        }
    }

    fn shadow_pattern(&mut self, node: Node<'a>, scope: usize) {
        if node.kind() == "identifier" {
            self.bind(scope, &self.source[node.byte_range()], Binding::Shadowed);
        } else if matches!(
            node.kind(),
            "pattern_list" | "tuple_pattern" | "list_pattern"
        ) {
            for child in node.named_children(&mut node.walk()) {
                self.shadow_pattern(child, scope);
            }
        }
    }

    fn import(&mut self, scope: usize, import: ImportStatement) {
        let ImportForm::Python { from_import, named } = &import.form else {
            return;
        };
        if *from_import {
            for specifier in named {
                self.bind(
                    scope,
                    imports::specifier_local_name(specifier),
                    Binding::Imported {
                        module: import.module_path.clone(),
                        symbol: Some(imports::specifier_imported_name(specifier).to_string()),
                        prefix: String::new(),
                    },
                );
            }
            self.imports.push(import);
        } else {
            for specifier in named {
                let module = imports::specifier_imported_name(specifier);
                let local = imports::specifier_local_name(specifier);
                let aliased = local != module;
                let name = if aliased {
                    local
                } else {
                    module.split('.').next().unwrap_or(module)
                };
                self.bind(
                    scope,
                    name,
                    Binding::Imported {
                        module: module.to_string(),
                        symbol: None,
                        prefix: if aliased {
                            local.to_string()
                        } else {
                            module.to_string()
                        },
                    },
                );
                let mut normalized = import.clone();
                normalized.module_path = module.to_string();
                normalized.namespace_import = Some(if aliased {
                    local.to_string()
                } else {
                    name.to_string()
                });
                self.imports.push(normalized);
            }
        }
    }
}
