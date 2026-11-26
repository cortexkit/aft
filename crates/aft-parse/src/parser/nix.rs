//! Nix outline extraction: the tree-sitter-nix binding walk that turns a
//! module file's attrpath bindings into outline symbols.
//!
//! Node-kind and field names in this module (`binding_set`,
//! `function_expression`, `formal`, `let_attrset_expression`, …) couple to the
//! pinned tree-sitter-nix version in Cargo.lock; a grammar bump can reshape
//! them, and the `nix_` tests in `parser.rs` are the pin that catches it.

use tree_sitter::Node;

use super::{node_range, node_text};
use crate::error::AftError;
use crate::symbols::{Symbol, SymbolKind};

/// Depth of nested bindings the Nix outline expands beneath their parent.
/// A named constant so widening it later does not touch the rest of the module.
const NIX_OUTLINE_DEPTH: usize = 1;

/// Upper bound on the name bytes one file's Nix outline may keep: symbol
/// names plus the parent and scope-chain copies each child carries.
///
/// Every child of a wide binding copies the parent name into two fields
/// (`parent` and the one-element `scope_chain`), so a file with one long
/// parent name and many small children retains about 2 × name length ×
/// child count bytes — a factor that grows with `NIX_OUTLINE_DEPTH` as
/// children copy the whole scope chain — quadratically more than the file
/// itself, which cold indexing would pay for a file even under the
/// scanner's read limit. Four MiB per file — the read limit itself —
/// leaves every real file whole (a normal outline keeps a small multiple
/// of the file's own attr-path bytes) and caps the pathological one; past
/// the bound the walk stops emitting, so the outline keeps its leading,
/// exact portion. The cap bounds retained name bytes, not total record
/// bytes: per-symbol overhead is uncharged and stays a linear factor of
/// the file size — what is ruled out is the super-linear name copy.
/// Stored signature bytes are bounded separately, by the
/// `MAX_SIGNATURE_BYTES` truncation every outline symbol passes through
/// in [`super::extract_symbols_from_tree_with`] before reaching a record.
pub(super) const NIX_OUTLINE_NAME_BUDGET: usize = 4 << 20;

/// Read an `attrpath` node as the dotted name written in the source.
fn nix_attrpath_name(source: &str, attrpath: &Node) -> String {
    let mut cursor = attrpath.walk();
    let parts: Vec<String> = attrpath
        .children_by_field_name("attr", &mut cursor)
        .map(|attr| node_text(source, &attr).trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    parts.join(".")
}

/// Unwrap parenthesized expressions until reaching the inner value.
fn nix_unwrap_parentheses(mut node: Node) -> Node {
    while node.kind() == "parenthesized_expression" {
        match node.child_by_field_name("expression") {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// The `binding_set` a value produces after unwrapping parentheses, a function
/// body, an application's final argument, or a `let … in` body. `None` when the
/// value is not an attribute set.
fn nix_value_binding_set(node: Node) -> Option<Node> {
    let mut current = nix_unwrap_parentheses(node);
    loop {
        match current.kind() {
            "function_expression" | "with_expression" | "assert_expression" => {
                current = current.child_by_field_name("body")?;
                current = nix_unwrap_parentheses(current);
            }
            "let_expression" => {
                current = current.child_by_field_name("body")?;
                current = nix_unwrap_parentheses(current);
            }
            // Module and package values are commonly wrapped in a call
            // (`config = lib.mkIf cond { … }`); the final argument is the
            // configuration attrset, the same rule the entry walk applies.
            "apply_expression" => {
                current = current.child_by_field_name("argument")?;
                current = nix_unwrap_parentheses(current);
            }
            "attrset_expression" | "rec_attrset_expression" | "let_attrset_expression" => {
                let mut cursor = current.walk();
                return current
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "binding_set");
            }
            _ => return None,
        }
    }
}

/// Collect byte ranges of comment nodes below `node`, skipping `body_id`. The
/// subtree branches, so the descent uses an explicit worklist rather than
/// recursion: a function whose formal defaults or non-body children nest deeply
/// would otherwise spend a stack frame per level and overflow a scan thread.
fn nix_comment_ranges(node: Node, body_id: Option<usize>, ranges: &mut Vec<(usize, usize)>) {
    let mut worklist = vec![node];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if Some(child.id()) == body_id {
                continue;
            }
            if child.kind() == "comment" {
                ranges.push((child.start_byte(), child.end_byte()));
            } else {
                worklist.push(child);
            }
        }
    }
}

/// `{ config, ... }:` or `x:` for a function expression, taken from the source
/// between the function's start and the end of its parameters.
fn nix_function_signature(source: &str, node: Node) -> Option<String> {
    let function = nix_unwrap_parentheses(node);
    if function.kind() != "function_expression" {
        return None;
    }
    // Take the header from the function start to the end of its parameters, so
    // a comment between the formals and the body stays out of the signature,
    // and splice comments that sit inside the formals out of the header.
    let body_id = function.child_by_field_name("body").map(|body| body.id());
    let mut end = function.start_byte();
    let mut cursor = function.walk();
    for child in function.named_children(&mut cursor) {
        if Some(child.id()) == body_id || child.kind() == "comment" {
            continue;
        }
        end = end.max(child.end_byte());
    }
    if end <= function.start_byte() {
        return None;
    }
    let start = function.start_byte();
    let mut comments = Vec::new();
    nix_comment_ranges(function, body_id, &mut comments);
    comments.sort_unstable();
    let mut header = String::new();
    let mut cursor_byte = start;
    for (comment_start, comment_end) in comments {
        if comment_start < cursor_byte || comment_end > end {
            continue;
        }
        header.push_str(source.get(cursor_byte..comment_start)?);
        cursor_byte = comment_end;
    }
    header.push_str(source.get(cursor_byte..end)?);
    // The outline renders a signature instead of the name, as it does for
    // `fn foo(…)`, so the header keeps the binding name and stays one line even
    // when the formals span several.
    let signature = header
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ,", ",");
    // Display-only collapse of wrapped-formal commas; it also rewrites a
    // literal " ," inside a formal default's string. Cosmetic on a
    // display field; quote-aware spans are out of scope here.
    Some(format!("{signature}:"))
}

fn nix_emit_binding(
    source: &str,
    binding: Node,
    scope_chain: &[String],
    parent: Option<&str>,
    depth: usize,
    budget: &mut usize,
    symbols: &mut Vec<Symbol>,
) {
    // Inherit clauses are aliases, not definitions; only `binding` nodes count.
    if binding.kind() != "binding" {
        return;
    }
    let Some(attrpath) = binding.child_by_field_name("attrpath") else {
        return;
    };
    let name = nix_attrpath_name(source, &attrpath);
    if name.is_empty() {
        return;
    }

    // Charge this symbol's name bytes before allocating anything they would
    // be copied into (`parent` and `scope_chain` below repeat the enclosing
    // name for every child); a binding whose copies no longer fit is dropped
    // without descending, so the walk stops spending work with the budget.
    let name_bytes = name.len()
        + parent.map(str::len).unwrap_or(0)
        + scope_chain.iter().map(String::len).sum::<usize>();
    if name_bytes > *budget {
        return;
    }
    *budget -= name_bytes;

    let value = binding.child_by_field_name("expression");
    let is_function = value
        .map(nix_unwrap_parentheses)
        .is_some_and(|node| node.kind() == "function_expression");
    let (kind, signature) = if is_function {
        (
            SymbolKind::Function,
            value
                .and_then(|v| nix_function_signature(source, v))
                .map(|header| format!("{name} = {header}")),
        )
    } else {
        (SymbolKind::Variable, None)
    };

    symbols.push(Symbol {
        name: name.clone(),
        kind,
        range: node_range(&binding),
        signature,
        scope_chain: scope_chain.to_vec(),
        exported: false,
        parent: parent.map(str::to_string),
    });

    if depth >= NIX_OUTLINE_DEPTH {
        return;
    }
    let Some(set) = value.and_then(nix_value_binding_set) else {
        return;
    };
    let mut child_scope = scope_chain.to_vec();
    child_scope.push(name.clone());
    let mut cursor = set.walk();
    let children: Vec<Node> = set.named_children(&mut cursor).collect();
    for child in children {
        nix_emit_binding(
            source,
            child,
            &child_scope,
            Some(&name),
            depth + 1,
            budget,
            symbols,
        );
    }
}

/// Collect entry binding sets, looking through wrappers, plus the bindings of a
/// top-level `let` block, which belong to the file rather than the body. The
/// wrappers form a single chain (one descent per level), so the walk is a loop
/// rather than recursion: a file of nested parens, `let … in`, or applied
/// functions would otherwise spend one stack frame per level and overflow the
/// 2 MiB stack a store-scan thread runs on.
fn nix_collect_entry<'a>(node: Node<'a>, sets: &mut Vec<Node<'a>>, loose: &mut Vec<Node<'a>>) {
    let mut node = node;
    loop {
        match node.kind() {
            "source_code" => {
                let Some(expression) = node.child_by_field_name("expression") else {
                    return;
                };
                node = expression;
            }
            "function_expression" | "with_expression" | "assert_expression" => {
                let Some(body) = node.child_by_field_name("body") else {
                    return;
                };
                node = body;
            }
            "parenthesized_expression" => {
                let Some(inner) = node.child_by_field_name("expression") else {
                    return;
                };
                node = inner;
            }
            "let_expression" => {
                let mut cursor = node.walk();
                if let Some(set) = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "binding_set")
                {
                    let mut set_cursor = set.walk();
                    loose.extend(set.named_children(&mut set_cursor));
                }
                let Some(body) = node.child_by_field_name("body") else {
                    return;
                };
                node = body;
            }
            // Package and module files usually return an applied function whose
            // last argument is the attrset that carries the bindings. The argument
            // goes through entry discovery itself so a `let … in { … }` argument
            // also contributes its own file-level bindings.
            "apply_expression" => {
                let Some(argument) = node.child_by_field_name("argument") else {
                    return;
                };
                node = argument;
            }
            "attrset_expression" | "rec_attrset_expression" | "let_attrset_expression" => {
                let mut cursor = node.walk();
                let set = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "binding_set");
                if let Some(set) = set {
                    sets.push(set);
                }
                return;
            }
            _ => return,
        }
    }
}

pub(super) fn extract_nix_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    let mut sets = Vec::new();
    let mut loose = Vec::new();
    nix_collect_entry(*root, &mut sets, &mut loose);

    let full_name = |binding: Node<'_>| {
        binding
            .child_by_field_name("attrpath")
            .map(|attrpath| nix_attrpath_name(source, &attrpath))
            .filter(|name| !name.is_empty())
    };

    // Nix attributes and `let` variables are separate namespaces, and two `let`
    // bindings can share a name too, so identical names are legal. The outline
    // keys nesting by parent name and cannot tell them apart: keep both
    // definitions and skip their nested expansion only when the full names are
    // identical.
    // Entry-set and `let` bindings are one flat file-level list: emit them in
    // source order, as every other language's outline does.
    let mut top_level: Vec<Node> = Vec::new();
    for set in sets {
        let mut cursor = set.walk();
        top_level.extend(set.named_children(&mut cursor));
    }
    top_level.extend(loose);
    top_level.sort_by_key(|binding| binding.start_byte());

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for binding in &top_level {
        if let Some(name) = full_name(*binding) {
            *counts.entry(name).or_default() += 1;
        }
    }
    let collided: std::collections::HashSet<&str> = counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| name.as_str())
        .collect();

    // One budget for the file: the walk charges every emitted symbol against
    // it, so a wide or huge file caps its retained names and the walk stops
    // early instead of copying an enclosing name once per child.
    let mut budget = NIX_OUTLINE_NAME_BUDGET;
    for binding in top_level {
        let depth = if full_name(binding).is_some_and(|name| collided.contains(name.as_str())) {
            NIX_OUTLINE_DEPTH
        } else {
            0
        };
        nix_emit_binding(source, binding, &[], None, depth, &mut budget, &mut symbols);
    }
    Ok(symbols)
}
