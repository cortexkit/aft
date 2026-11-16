//! Extraction for the pinned Luau grammar, independent of file discovery.

use std::collections::{BTreeMap, HashMap};

use tree_sitter::Node;

use crate::imports::{ImportBlock, ImportForm, ImportGroup, ImportKind, ImportStatement};
use crate::symbols::SymbolKind;

// Validate the syntax used by extraction before marking the Luau grammar ready.
// The grammar registry reads these on every target that builds downloaded
// grammars, Windows ARM64 included.
#[cfg(feature = "wasm-grammars")]
pub const EXTRACTION_QUERY: &str = r#"
[(function_declaration name: (_) @name)
 (local_function_declaration name: (identifier) @name)
 (local_variable_declaration (binding_list (binding name: (identifier) @name)))
 (assignment_statement (variable_list) (expression_list))
 (type_alias_declaration name: (type_identifier) @name)
 (function_definition) (table_constructor (field))
 (return_statement) (function_call name: (_) arguments: (arguments))] @declaration
"#;
#[cfg(feature = "wasm-grammars")]
pub const COMPLEXITY_QUERY: &str = r#"
[(function_declaration) (local_function_declaration) (function_definition)
 (if_statement) (elseif_statement) (if_else_expression) (elseif_expression)
 (for_statement) (while_statement) (repeat_statement) (binary_expression)] @complexity
"#;
#[cfg(feature = "wasm-grammars")]
pub const COMMENT_QUERY: &str = "(comment content: (comment_content) @content) @comment";
pub const COMMENT_KINDS: &[&str] = &["comment"];

pub fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut cursor = root.walk();
    loop {
        result.push(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return result;
            }
        }
    }
}

fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

fn child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    node.named_children(&mut node.walk())
        .find(|n| n.kind() == kind)
}

fn single(node: Node<'_>) -> Option<Node<'_>> {
    (node.named_child_count() == 1)
        .then(|| node.named_child(0))
        .flatten()
}

pub(crate) fn local_pair(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let binding = single(child(node, "binding_list")?)?;
    Some((
        binding.child_by_field_name("name")?,
        single(child(node, "expression_list")?)?,
    ))
}

pub(crate) fn assignment_pairs(node: Node<'_>) -> Vec<(Node<'_>, Option<Node<'_>>)> {
    let Some(left) = child(node, "variable_list") else {
        return Vec::new();
    };
    let right = child(node, "expression_list");
    left.named_children(&mut left.walk())
        .enumerate()
        .map(|(i, name)| (name, right.and_then(|r| r.named_child(i as u32))))
        .collect()
}

/// Value-flow follows only one plain binding, not parallel assignments.
pub(crate) fn single_assignment_pair(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let value = single(child(node, "expression_list")?)?;
    let pairs = assignment_pairs(node);
    match pairs.as_slice() {
        [(name, Some(_))] if name.kind() == "identifier" => Some((*name, value)),
        _ => None,
    }
}

fn member<'source, 'tree>(
    source: &'source str,
    node: Node<'tree>,
) -> Option<(&'source str, Node<'tree>)> {
    if !matches!(
        node.kind(),
        "dot_index_expression" | "method_index_expression"
    ) {
        return None;
    }
    let table = node.child_by_field_name("table")?;
    let name = node
        .child_by_field_name("field")
        .or_else(|| node.child_by_field_name("method"))?;
    Some((text(source, table), name))
}

pub struct Declaration<'a> {
    pub name: Node<'a>,
    pub definition: Node<'a>,
    pub kind: SymbolKind,
    pub parent: Option<String>,
    pub exported: bool,
}

pub fn declarations<'a>(source: &str, root: Node<'a>) -> Vec<Declaration<'a>> {
    let mut declarations = Vec::new();
    for node in nodes(root) {
        match node.kind() {
            "function_declaration" | "local_function_declaration" => {
                let Some(name) = node.child_by_field_name("name") else {
                    continue;
                };
                let (parent, name) = member(source, name)
                    .map(|(parent, name)| (Some(parent.to_string()), name))
                    .unwrap_or((None, name));
                declarations.push(Declaration {
                    name,
                    definition: node,
                    kind: if parent.is_some() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    },
                    parent,
                    exported: true,
                });
            }
            "assignment_statement" => {
                for (left, right) in assignment_pairs(node) {
                    if !right.is_some_and(|r| r.kind() == "function_definition") {
                        continue;
                    }
                    if let Some((parent, name)) = member(source, left) {
                        declarations.push(Declaration {
                            name,
                            definition: node,
                            kind: SymbolKind::Method,
                            parent: Some(parent.to_string()),
                            exported: true,
                        });
                    }
                }
            }
            "local_variable_declaration" if node.parent() == Some(root) => {
                if let Some(bindings) = child(node, "binding_list") {
                    for binding in bindings.named_children(&mut bindings.walk()) {
                        if let Some(name) = binding.child_by_field_name("name") {
                            declarations.push(Declaration {
                                name,
                                definition: node,
                                kind: SymbolKind::Variable,
                                parent: None,
                                exported: true,
                            });
                        }
                    }
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    declarations.push(Declaration {
                        name,
                        definition: node,
                        kind: SymbolKind::TypeAlias,
                        parent: None,
                        exported: text(source, node).starts_with("export"),
                    });
                }
            }
            _ => {}
        }
    }
    declarations
}

/// A literal string without escape interpretation. Escaped paths are deliberately
/// unresolved rather than risking a different filename from Luau's interpretation.
fn literal(source: &str, node: Node<'_>) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let value = text(source, node);
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"') || !value.ends_with(quote) || value.len() < 2 {
        return None;
    }
    let value = &value[1..value.len() - 1];
    (!value.contains('\\')).then(|| value.to_string())
}

// None is the Parent property; Some is a literal child lookup.
fn script_steps(source: &str, mut node: Node<'_>) -> Option<Vec<Option<String>>> {
    let mut steps = Vec::new();
    loop {
        if node.kind() == "identifier" && text(source, node) == "script" {
            steps.reverse();
            return Some(steps);
        }
        let (table, step) = match node.kind() {
            "dot_index_expression" => {
                let field = text(source, node.child_by_field_name("field")?);
                (
                    node.child_by_field_name("table")?,
                    (field != "Parent").then(|| field.to_string()),
                )
            }
            "bracket_index_expression" => (
                node.child_by_field_name("table")?,
                Some(literal(source, node.child_by_field_name("field")?)?),
            ),
            "function_call" => {
                let callee = node.child_by_field_name("name")?;
                if callee.kind() != "method_index_expression"
                    || !matches!(
                        text(source, callee.child_by_field_name("method")?),
                        "WaitForChild" | "FindFirstChild"
                    )
                {
                    return None;
                }
                (
                    callee.child_by_field_name("table")?,
                    Some(literal(
                        source,
                        single(node.child_by_field_name("arguments")?)?,
                    )?),
                )
            }
            _ => return None,
        };
        steps.push(step);
        node = table;
    }
}

pub const SCRIPT_PREFIX: &str = "__luau_script:";

pub fn imports(source: &str, root: Node<'_>) -> ImportBlock {
    let all = nodes(root);
    let mut imports = Vec::new();
    for &node in &all {
        if node.kind() != "function_call"
            || !node
                .child_by_field_name("name")
                .is_some_and(|n| n.kind() == "identifier" && text(source, n) == "require")
        {
            continue;
        }
        let Some(argument) = node.child_by_field_name("arguments").and_then(single) else {
            continue;
        };
        let module_path = literal(source, argument)
            .map(|path| {
                if path.starts_with(SCRIPT_PREFIX) {
                    format!("!{path}")
                } else {
                    path
                }
            })
            .or_else(|| {
                script_steps(source, argument).map(|steps| {
                    format!(
                        "{SCRIPT_PREFIX}{}",
                        serde_json::to_string(&steps).expect("script steps")
                    )
                })
            })
            .unwrap_or_else(|| text(source, argument).to_string());
        let statement = node
            .parent()
            .filter(|n| n.kind() == "expression_list")
            .and_then(|n| n.parent())
            .filter(|n| n.kind() == "local_variable_declaration");
        let binding = statement
            .and_then(local_pair)
            .filter(|(_, value)| *value == node)
            .map(|(name, _)| text(source, name).to_string())
            .filter(|name| !reassigned(source, &all, name))
            .filter(|name| {
                statement.is_some_and(|declaration| {
                    binding_covers_calls(source, &all, declaration, name)
                })
            });
        let span = statement.unwrap_or(node);
        imports.push(ImportStatement {
            module_path,
            names: Vec::new(),
            default_import: binding.clone(),
            namespace_import: None,
            kind: if binding.is_some() {
                ImportKind::Value
            } else {
                ImportKind::SideEffect
            },
            group: ImportGroup::Internal,
            byte_range: span.byte_range(),
            raw_text: text(source, span).to_string(),
            form: ImportForm::Structured {
                named: Vec::new(),
                namespace: None,
                alias: binding,
                modifiers: Vec::new(),
                import_kind: None,
            },
        });
    }
    let byte_range = imports
        .first()
        .zip(imports.last())
        .map(|(a, b)| a.byte_range.start..b.byte_range.end);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn binding_covers_calls(source: &str, all: &[Node<'_>], declaration: Node<'_>, name: &str) -> bool {
    let Some(scope) = declaration.parent() else {
        return false;
    };
    // References retain a local's name rather than its lexical scope. Keep a
    // binding only when every matching member call can see that declaration;
    // otherwise resolution could mistake a same-named global for the imported local.
    all.iter()
        .filter(|n| n.kind() == "function_call")
        .filter(|n| {
            n.child_by_field_name("name")
                .and_then(|n| member(source, n))
                .is_some_and(|(owner, _)| owner == name)
        })
        .all(|call| {
            if call.start_byte() < declaration.end_byte() {
                return false;
            }
            let mut ancestor = call.parent();
            while let Some(node) = ancestor {
                if node == scope {
                    return true;
                }
                ancestor = node.parent();
            }
            false
        })
}

fn reassigned(source: &str, all: &[Node<'_>], name: &str) -> bool {
    let declarations = all
        .iter()
        .filter(|n| {
            matches!(n.kind(), "binding" | "local_function_declaration")
                && n.child_by_field_name("name")
                    .is_some_and(|n| text(source, n) == name)
        })
        .count();
    declarations > 1
        || all.iter().any(|&n| match n.kind() {
            "function_declaration" => n
                .child_by_field_name("name")
                .is_some_and(|left| left.kind() == "identifier" && text(source, left) == name),
            "assignment_statement" => assignment_pairs(n)
                .iter()
                .any(|(left, _)| left.kind() == "identifier" && text(source, *left) == name),
            "compound_assignment_statement" => n
                .child_by_field_name("left")
                .is_some_and(|left| text(source, left) == name),
            _ => false,
        })
}

pub struct Export<'a> {
    pub symbol: String,
    pub inline: Option<Declaration<'a>>,
}

fn chunk_level(mut node: Node<'_>, root: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent == root {
            return true;
        }
        if matches!(
            parent.kind(),
            "function_declaration" | "local_function_declaration" | "function_definition"
        ) {
            return false;
        }
        node = parent;
    }
    false
}

/// Only the final, unconditional chunk return defines the module surface. Count
/// writes before choosing functions: a later nil assignment is not an export.
pub fn exports<'a>(source: &str, root: Node<'a>) -> BTreeMap<String, Export<'a>> {
    if root.has_error() {
        return BTreeMap::new();
    }
    let all = nodes(root);
    let returns = all
        .iter()
        .copied()
        .filter(|&n| n.kind() == "return_statement" && chunk_level(n, root))
        .collect::<Vec<_>>();
    if returns.len() != 1 {
        return BTreeMap::new();
    }
    let ret = returns[0];
    if ret.parent() != Some(root)
        || root
            .named_children(&mut root.walk())
            .filter(|n| n.kind() != "comment")
            .last()
            != Some(ret)
    {
        return BTreeMap::new();
    }
    let Some(value) = child(ret, "expression_list").and_then(single) else {
        return BTreeMap::new();
    };
    let (table_name, table) = match value.kind() {
        "table_constructor" => (None, value),
        "identifier" => {
            let name = text(source, value);
            let locals = root
                .named_children(&mut root.walk())
                .filter(|n| n.kind() == "local_variable_declaration")
                .filter_map(local_pair)
                .filter(|(n, _)| text(source, *n) == name)
                .collect::<Vec<_>>();
            if locals.len() != 1
                || locals[0].1.kind() != "table_constructor"
                || reassigned(source, &all, name)
            {
                return BTreeMap::new();
            }
            (Some(name), locals[0].1)
        }
        _ => return BTreeMap::new(),
    };
    let functions = root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "local_function_declaration")
        .filter_map(|n| {
            n.child_by_field_name("name")
                .map(|name| (text(source, name), n))
        })
        .collect::<HashMap<_, _>>();
    let parent = table_name.unwrap_or("<return>");
    let mut writes = BTreeMap::<String, Vec<Option<Export<'a>>>>::new();
    for field in table
        .named_children(&mut table.walk())
        .filter(|n| n.kind() == "field")
    {
        let Some(name) = field.child_by_field_name("name") else {
            continue;
        };
        // A computed key may alias any named member. Do not guess its value.
        if name.kind() != "field_identifier" {
            return BTreeMap::new();
        }
        let name_text = text(source, name).to_string();
        let export = field.child_by_field_name("value").and_then(|value| {
            if value.kind() == "function_definition" {
                Some(Export {
                    symbol: format!("{parent}::{name_text}"),
                    inline: Some(Declaration {
                        name,
                        definition: value,
                        kind: SymbolKind::Method,
                        parent: Some(parent.to_string()),
                        exported: false,
                    }),
                })
            } else if value.kind() == "identifier"
                && functions.contains_key(text(source, value))
                && !reassigned(source, &all, text(source, value))
            {
                Some(Export {
                    symbol: text(source, value).to_string(),
                    inline: None,
                })
            } else {
                None
            }
        });
        writes.entry(name_text).or_default().push(export);
    }
    if let Some(table_name) = table_name {
        for &node in &all {
            match node.kind() {
                "function_declaration" => {
                    let Some(name) = node.child_by_field_name("name") else {
                        continue;
                    };
                    if let Some((owner, name)) = member(source, name) {
                        if owner == table_name {
                            let key = text(source, name).to_string();
                            let export = (node.parent() == Some(root)).then(|| Export {
                                symbol: format!("{owner}::{key}"),
                                inline: None,
                            });
                            writes.entry(key).or_default().push(export);
                        }
                    }
                }
                "assignment_statement" => {
                    for (left, right) in assignment_pairs(node) {
                        if left.kind() == "bracket_index_expression"
                            && left
                                .child_by_field_name("table")
                                .is_some_and(|t| text(source, t) == table_name)
                        {
                            return BTreeMap::new();
                        }
                        if let Some((owner, name)) = member(source, left) {
                            if owner == table_name {
                                let key = text(source, name).to_string();
                                let export = (node.parent() == Some(root)
                                    && right.is_some_and(|r| r.kind() == "function_definition"))
                                .then(|| Export {
                                    symbol: format!("{owner}::{key}"),
                                    inline: None,
                                });
                                writes.entry(key).or_default().push(export);
                            }
                        }
                    }
                }
                "compound_assignment_statement" => {
                    if let Some((owner, name)) = node
                        .child_by_field_name("left")
                        .and_then(|n| member(source, n))
                    {
                        if owner == table_name {
                            writes
                                .entry(text(source, name).to_string())
                                .or_default()
                                .push(None);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    writes
        .into_iter()
        .filter_map(|(name, mut entries)| {
            (entries.len() == 1)
                .then(|| entries.pop().flatten())
                .flatten()
                .map(|export| (name, export))
        })
        .collect()
}
