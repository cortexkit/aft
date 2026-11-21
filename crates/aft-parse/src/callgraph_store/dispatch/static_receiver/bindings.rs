//! Value bindings that veto static receiver inference without changing the
//! existing instance-receiver inference.

use tree_sitter::Node;

use super::super::{field, Nav, FUNCTIONS};

const LOCAL_FUNCTIONS: &[&str] = &[
    "local_function_statement",
    "constructor_declaration",
    "accessor_declaration",
    "destructor_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
    "compact_constructor_declaration",
    "generator_function_declaration",
];

pub(super) fn enclosing_function<'tree>(
    node: Node<'tree>,
    nav: impl Nav<'tree>,
) -> Option<Node<'tree>> {
    let mut current = Some(node);
    while let Some(node) = current {
        if FUNCTIONS.contains(&node.kind()) || LOCAL_FUNCTIONS.contains(&node.kind()) {
            return Some(node);
        }
        current = nav.parent(node);
    }
    None
}

pub(super) fn names<'tree>(node: Node<'tree>, language: &str) -> Vec<Node<'tree>> {
    let mut names = Vec::new();
    let kind = node.kind();
    match language {
        "csharp" => match kind {
            "variable_declarator" => collect(
                field(node, &["name"]).or_else(|| child(node, "tuple_pattern")),
                &mut names,
            ),
            "parameter"
            | "local_function_statement"
            | "property_declaration"
            | "event_declaration"
            | "enum_member_declaration"
            | "declaration_expression"
            | "declaration_pattern"
            | "recursive_pattern"
            | "var_pattern"
            | "from_clause"
            | "catch_declaration" => collect(field(node, &["name"]), &mut names),
            "type_parameter" => collect(
                field(node, &["name"]).or_else(|| child(node, "identifier")),
                &mut names,
            ),
            "foreach_statement" => collect(field(node, &["left"]), &mut names),
            "lambda_expression" => collect(
                field(node, &["parameters"]).filter(|node| node.kind() == "implicit_parameter"),
                &mut names,
            ),
            "let_clause" | "join_clause" | "join_into_clause" => {
                collect(child(node, "identifier"), &mut names)
            }
            "parenthesized_variable_designation" => collect(Some(node), &mut names),
            // `into name` after select/group is a token and an identifier on the query.
            "query_expression" => {
                let mut cursor = node.walk();
                let mut nodes = node.children(&mut cursor).peekable();
                while let Some(child) = nodes.next() {
                    if child.kind() == "into" {
                        collect(
                            nodes.peek().copied().filter(|n| n.kind() == "identifier"),
                            &mut names,
                        );
                    }
                }
            }
            _ => {}
        },
        "java" => match kind {
            "variable_declarator"
            | "formal_parameter"
            | "catch_formal_parameter"
            | "enum_constant"
            | "enhanced_for_statement"
            | "resource"
            | "instanceof_expression" => collect(field(node, &["name"]), &mut names),
            "spread_parameter" => {} // Its variable_declarator is visited separately.
            "lambda_expression" => collect(
                field(node, &["parameters"]).filter(|node| node.kind() != "formal_parameters"),
                &mut names,
            ),
            "record_pattern_component" | "type_pattern" => {
                // The type can also contain identifiers; only the trailing name binds.
                collect(
                    node.named_children(&mut node.walk())
                        .last()
                        .filter(|n| n.kind() == "identifier"),
                    &mut names,
                );
            }
            _ => {}
        },
        _ => match kind {
            "variable_declarator" => collect(field(node, &["name"]), &mut names),
            "required_parameter" | "optional_parameter" => {
                collect(field(node, &["pattern", "name"]), &mut names)
            }
            "formal_parameters" => {
                // JS parameters are raw patterns; TS wrappers are visited separately.
                for pattern in node.named_children(&mut node.walk()) {
                    collect(Some(pattern), &mut names);
                }
            }
            "arrow_function" => collect(field(node, &["parameter"]), &mut names),
            "for_in_statement" | "for_of_statement" if field(node, &["kind"]).is_some() => {
                collect(field(node, &["left"]), &mut names)
            }
            "catch_clause" => collect(field(node, &["parameter"]), &mut names),
            "public_field_definition" | "field_definition" => {
                collect(field(node, &["name", "property"]), &mut names)
            }
            _ => {}
        },
    }
    names
}

fn child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    node.named_children(&mut node.walk())
        .find(|n| n.kind() == kind)
}

fn collect<'tree>(node: Option<Node<'tree>>, names: &mut Vec<Node<'tree>>) {
    let mut pending = node.into_iter().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier"
            | "implicit_parameter"
            | "property_identifier"
            | "shorthand_property_identifier_pattern" => names.push(node),
            "pair_pattern" => pending.extend(field(node, &["value"])),
            "assignment_pattern" | "object_assignment_pattern" => {
                pending.extend(field(node, &["left"]))
            }
            "tuple_pattern"
            | "parenthesized_variable_designation"
            | "inferred_parameters"
            | "array_pattern"
            | "object_pattern"
            | "rest_pattern" => pending.extend(node.named_children(&mut node.walk())),
            _ => {}
        }
    }
}

/// Use the loop/handler/query scope for declarations local to a construct.
/// Pattern and out variables can outlive the expression introducing them;
/// use the enclosing callable when their exact scope cannot be determined.
pub(super) fn scope<'tree>(
    node: Node<'tree>,
    language: &str,
    nav: impl Nav<'tree>,
) -> Option<Node<'tree>> {
    match node.kind() {
        // Generic parameter names are visible in their own declaration,
        // including nested types and callables.
        "type_parameter" if language == "csharp" => nav
            .parent(node)
            .filter(|parent| parent.kind() == "type_parameter_list")
            .and_then(|list| nav.parent(list)),
        "local_function_statement" => enclosing_function(nav.parent(node)?, nav),
        "resource" => nav.enclosing(node, &["try_with_resources_statement"]),
        "catch_formal_parameter" | "catch_declaration" => nav.enclosing(node, &["catch_clause"]),
        "from_clause" | "let_clause" | "join_clause" | "join_into_clause" => {
            nav.enclosing(node, &["query_expression"])
        }
        "foreach_statement" | "enhanced_for_statement" | "catch_clause" | "query_expression" => {
            Some(node)
        }
        "for_in_statement" | "for_of_statement"
            if field(node, &["kind"]).is_some_and(|n| n.kind() != "var") =>
        {
            Some(node)
        }
        "variable_declarator" if !matches!(language, "csharp" | "java") => {
            let parent = nav.parent(node)?;
            if parent.kind() == "variable_declaration"
                && parent
                    .children(&mut parent.walk())
                    .any(|n| n.kind() == "var")
            {
                enclosing_function(node, nav).or_else(|| nav.enclosing(node, &["program"]))
            } else {
                nav.enclosing(
                    node,
                    &[
                        "statement_block",
                        "for_statement",
                        "for_in_statement",
                        "program",
                    ],
                )
            }
        }
        _ => enclosing_function(node, nav),
    }
}
