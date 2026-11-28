use super::{export_specifier_names, find_child_by_kind, node_text, signature};
use crate::callgraph_store::join::syntax_index::Nav;
use crate::symbols::{Symbol, SymbolKind};
use std::collections::HashSet;
use tree_sitter::Node;

pub(super) const QUERY: &str = r#"
;; function declarations
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; function-like values assigned to const/let/var
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (arrow_function) @arrow.body) @arrow.decl) @arrow.def
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (function_expression) @arrow.body) @arrow.decl) @arrow.def
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (generator_function) @arrow.body) @arrow.decl) @arrow.def

;; anonymous default exports
(export_statement
  value: (function_expression) @default.body) @default.def
(export_statement
  value: (generator_function) @default.body) @default.def
(export_statement
  value: (class) @default.body) @default.def

;; class declarations
(class_declaration
  name: (type_identifier) @class.name) @class.def

;; method definitions inside classes
(class_declaration
  name: (type_identifier) @method.class_name
  body: (class_body
    (method_definition
      name: (property_identifier) @method.name) @method.def))

;; callable fields inside classes
(class_declaration
  name: (type_identifier) @class_property.class_name
  body: (class_body
    (public_field_definition
      name: (_) @class_property.name
      value: (arrow_function) @class_property.value) @class_property.def))
(class_declaration
  name: (type_identifier) @class_property.class_name
  body: (class_body
    (public_field_definition
      name: (_) @class_property.name
      value: (function_expression) @class_property.value) @class_property.def))

;; interface declarations and callable members
(interface_declaration
  name: (type_identifier) @interface.name) @interface.def
(interface_declaration
  name: (type_identifier) @interface_member.interface_name
  body: (interface_body
    (method_signature
      name: (_) @interface_member.name) @interface_member.def))
(interface_declaration
  name: (type_identifier) @interface_property.interface_name
  body: (interface_body
    (property_signature
      name: (_) @interface_property.name) @interface_property.def))

;; callable object-literal properties
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (pair
        key: (_) @object_property.name
        value: (arrow_function) @object_property.value) @object_property.def)))
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (pair
        key: (_) @object_property.name
        value: (function_expression) @object_property.value) @object_property.def)))
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (pair
        key: (_) @object_property.name
        value: (generator_function) @object_property.value) @object_property.def)))
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (method_definition
        name: (_) @object_property.name) @object_property.def)))

;; top-level enums and type aliases
(enum_declaration
  name: (identifier) @enum.name) @enum.def
(type_alias_declaration
  name: (type_identifier) @type_alias.name) @type_alias.def

;; variable declarations; patterns emit only their exported bindings
(lexical_declaration
  (variable_declarator
    name: [(identifier) (object_pattern) (array_pattern)] @var.name) @var.decl) @var.def
(variable_declaration
  (variable_declarator
    name: [(object_pattern) (array_pattern)] @var.name) @var.decl) @var.def

;; export wrappers for the range-index reference test
(export_statement) @export.stmt
"#;

/// Local bindings exported by an export clause (`export { a, b as c }`) or a
/// default identifier (`export default a`).
pub(super) fn named_exports(source: &str, root: Node<'_>) -> HashSet<String> {
    let mut names = HashSet::new();
    for statement in root.named_children(&mut root.walk()) {
        if statement.kind() != "export_statement"
            || statement.child_by_field_name("source").is_some()
        {
            continue;
        }
        if let Some(clause) = find_child_by_kind(statement, "export_clause") {
            for specifier in clause.named_children(&mut clause.walk()) {
                if specifier.kind() == "export_specifier" {
                    if let Some((local, _)) = export_specifier_names(source, &specifier) {
                        names.insert(local);
                    }
                }
            }
        } else if let Some(value) = statement.child_by_field_name("value") {
            if value.kind() == "identifier" {
                names.insert(node_text(source, &value).to_owned());
            }
        }
    }
    names
}

/// Only the export statement's declaration is exported, not declarations in
/// its initializer or body. An ambient `declare` wraps the same declaration.
fn export_status<'tree>(mut declaration: Node<'tree>, nav: impl Nav<'tree>) -> (bool, bool) {
    let mut parent = nav.parent(declaration);
    if let Some(ambient) = parent.filter(|node| node.kind() == "ambient_declaration") {
        declaration = ambient;
        parent = nav.parent(ambient);
    }
    let direct = parent.is_some_and(|node| {
        node.kind() == "export_statement"
            && node.child_by_field_name("declaration") == Some(declaration)
    });
    let module_scope = parent.is_some_and(|node| node.kind() == "program");
    (direct, module_scope)
}

pub(super) fn is_exported<'tree>(
    declaration: Node<'tree>,
    name: &str,
    named_exports: &HashSet<String>,
    nav: impl Nav<'tree>,
) -> bool {
    let (direct, module_scope) = export_status(declaration, nav);
    direct || (module_scope && named_exports.contains(name))
}

pub(super) fn push_pattern_symbols<'tree>(
    symbols: &mut Vec<Symbol>,
    source: &str,
    pattern: Node<'tree>,
    declarator: Node<'tree>,
    declaration: Node<'tree>,
    named_exports: &HashSet<String>,
    nav: impl Nav<'tree>,
) {
    let (direct, module_scope) = export_status(declaration, nav);
    if !direct && (!module_scope || named_exports.is_empty()) {
        return;
    }
    let keyword = if declaration.kind() == "variable_declaration" {
        "var"
    } else {
        declaration
            .child_by_field_name("kind")
            .map_or("const", |node| node.kind())
    };
    // Each binding uses the whole declarator so zoom also shows its initializer.
    let range = super::node_range(&declarator);
    let mut pending = vec![pattern];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                let name = node_text(source, &node);
                if direct || named_exports.contains(name) {
                    symbols.push(Symbol {
                        name: name.to_owned(),
                        kind: SymbolKind::Variable,
                        range: range.clone(),
                        signature: Some(signature::bounded_parts([keyword, " ", name])),
                        scope_chain: vec![],
                        exported: true,
                        parent: None,
                    });
                }
            }
            // The key and the default value are expressions, not bindings.
            "pair_pattern" => pending.extend(node.child_by_field_name("value")),
            "assignment_pattern" | "object_assignment_pattern" => {
                pending.extend(node.child_by_field_name("left"));
            }
            "object_pattern" | "array_pattern" | "rest_pattern" => {
                let start = pending.len();
                pending.extend(node.named_children(&mut node.walk()));
                pending[start..].reverse();
            }
            _ => {}
        }
    }
}
