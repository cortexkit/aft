//! Python imports use lexical bindings and dotted module paths, not JS specifiers.

use crate::imports::{self, ImportBlock, ImportForm, ImportStatement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

/// A lexically selected binding for one call. A missing target forbids name-based
/// fallback when a local name shadows an import or a declaration is ambiguous.
/// An empty target module names a declaration in the current file.
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
    LocalDefinition {
        symbol: String,
        position: usize,
        class: bool,
    },
    Shadowed,
}

struct Scope {
    parent: Option<usize>,
    class: bool,
    owner: String,
    classes: Vec<String>,
    names: HashMap<String, Binding>,
}

struct Extractor<'a> {
    source: &'a str,
    scopes: Vec<Scope>,
    imports: Vec<ImportStatement>,
    module_imports: HashSet<(usize, usize)>,
    declarations: HashMap<String, usize>,
    calls: Vec<(Node<'a>, usize)>,
    module_only: bool,
}

pub(crate) struct Extraction {
    pub imports: ImportBlock,
    pub bindings: Vec<CallBinding>,
    pub owners: HashMap<usize, Vec<String>>,
}

pub(crate) fn extract(source: &str, root: Node<'_>) -> Extraction {
    let mut extractor = Extractor::new(source, false);
    extractor.visit(root, 0);
    let mut bindings: Vec<_> = extractor
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
            let target = match binding {
                Binding::LocalDefinition { symbol, class, .. }
                    if parts.len() == 1
                        && (!*class || extractor.declarations.get(symbol) == Some(&1)) =>
                {
                    Some((String::new(), symbol.clone()))
                }
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
    bindings.sort_by_key(|binding| (binding.byte_start, binding.byte_end));
    Extraction {
        imports: ImportBlock {
            imports: extractor.imports,
            byte_range: None,
        },
        bindings,
        owners: extractor
            .calls
            .iter()
            .map(|(call, scope)| {
                let mut owners = vec![extractor.scopes[*scope].owner.clone()];
                let mut parent = extractor.scopes[*scope].parent;
                // Like other languages, retain enclosing callable edges for nested
                // bodies. Start from the lexical scope so declaration headers still
                // belong to their defining scope, never the declared function.
                while let Some(id) = parent {
                    let enclosing = &extractor.scopes[id];
                    if !enclosing.class
                        && enclosing.owner != super::TOP_LEVEL_SYMBOL
                        && !owners.contains(&enclosing.owner)
                    {
                        owners.push(enclosing.owner.clone());
                    }
                    parent = enclosing.parent;
                }
                (call.start_byte(), owners)
            })
            .collect(),
    }
}

pub(crate) fn reexports(source: &str, root: Node<'_>) -> Vec<ImportStatement> {
    let mut extractor = Extractor::new(source, true);
    extractor.visit(root, 0);
    let mut reexports = Vec::new();
    for import in &extractor.imports {
        if !extractor
            .module_imports
            .contains(&(import.byte_range.start, import.byte_range.end))
        {
            continue;
        }
        let ImportForm::Python {
            from_import: true,
            named,
        } = &import.form
        else {
            continue;
        };
        let named = named.iter().filter(|specifier| {
            let local = imports::specifier_local_name(specifier);
            matches!(extractor.scopes[0].names.get(local), Some(Binding::Imported { module, symbol: Some(symbol), .. })
                if module == &import.module_path && symbol == imports::specifier_imported_name(specifier))
        }).cloned().collect::<Vec<_>>();
        if !named.is_empty() {
            let mut exported = import.clone();
            exported.names = named.clone();
            exported.form = ImportForm::Python {
                from_import: true,
                named,
            };
            reexports.push(exported);
        }
    }
    reexports
}

impl<'a> Extractor<'a> {
    fn new(source: &'a str, module_only: bool) -> Self {
        Self {
            source,
            scopes: vec![Scope {
                parent: None,
                class: false,
                owner: super::TOP_LEVEL_SYMBOL.to_string(),
                classes: Vec::new(),
                names: HashMap::new(),
            }],
            imports: Vec::new(),
            module_imports: HashSet::new(),
            declarations: HashMap::new(),
            calls: Vec::new(),
            module_only,
        }
    }

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

    fn visit(&mut self, node: Node<'a>, scope: usize) {
        if matches!(
            node.kind(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                let mut path = self.scopes[scope].classes.clone();
                path.push(self.source[name.byte_range()].to_string());
                // Python symbol identities omit enclosing function names. Distinct
                // function-local classes can therefore share a stored name;
                // constructor calls must stay unresolved when that name is ambiguous.
                *self.declarations.entry(path.join("::")).or_default() += 1;
                self.bind(
                    scope,
                    &self.source[name.byte_range()],
                    Binding::LocalDefinition {
                        symbol: path.join("::"),
                        position: node.start_byte(),
                        class: node.kind() == "class_definition",
                    },
                );
            }
            if self.module_only {
                // A module's exports do not depend on function or class locals.
                // Headers still run here and can rebind names with assignment expressions.
                let body = node.child_by_field_name("body");
                for child in node.named_children(&mut node.walk()) {
                    if Some(child) != body {
                        self.visit(child, scope);
                    }
                }
                return;
            }
            let id = self.scopes.len();
            let mut classes = self.scopes[scope].classes.clone();
            let owner = if let Some(name) = node.child_by_field_name("name") {
                let name = &self.source[name.byte_range()];
                let mut path = classes.clone();
                path.push(name.to_string());
                if node.kind() == "class_definition" {
                    classes.push(name.to_string());
                }
                path.join("::")
            } else {
                self.scopes[scope].owner.clone()
            };
            self.scopes.push(Scope {
                parent: Some(scope),
                class: node.kind() == "class_definition",
                owner,
                classes,
                names: HashMap::new(),
            });
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
                        self.bind(id, &self.source[name.byte_range()], Binding::Shadowed);
                    }
                }
            }
            // Defaults, annotations, bases and decorators execute where the
            // declaration is defined; only its body sees the new local scope.
            let body = node.child_by_field_name("body");
            for child in node.named_children(&mut node.walk()) {
                self.visit(child, if Some(child) == body { id } else { scope });
            }
            return;
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
        if node.kind() == "as_pattern" {
            if let Some(alias) = node.child_by_field_name("alias") {
                self.shadow_pattern(alias, scope);
            }
        }
        if node.kind() == "delete_statement" {
            for child in node.named_children(&mut node.walk()) {
                self.shadow_pattern(child, scope);
            }
        }
        if node.kind() == "call" && !self.module_only {
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
            "pattern_list" | "tuple_pattern" | "list_pattern" | "as_pattern_target"
        ) {
            for child in node.named_children(&mut node.walk()) {
                self.shadow_pattern(child, scope);
            }
        }
    }

    fn import(&mut self, scope: usize, import: ImportStatement) {
        if scope == 0 {
            self.module_imports
                .insert((import.byte_range.start, import.byte_range.end));
        }
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
aft_store::heap_bytes_of_fields!(CallBinding {
    byte_start,
    byte_end,
    target,
});
