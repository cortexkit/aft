use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

const BARE_STRING_ARGUMENT_MODIFIER: &str = "bare_string_argument";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequireArgumentStyle {
    Parenthesized,
    BareString,
}

struct LuaRequireCall {
    module_path: String,
    argument_style: RequireArgumentStyle,
}

pub fn classify_group_lua(_module_path: &str) -> ImportGroup {
    ImportGroup::External
}

pub fn parse_lua_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            match node.kind() {
                "function_call" => {
                    if let Some(imp) = parse_lua_bare_require(source, &node) {
                        imports.push(imp);
                    }
                }
                "variable_declaration" => {
                    if let Some(imp) = parse_lua_local_require(source, &node) {
                        imports.push(imp);
                    }
                }
                _ => {}
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn parse_lua_bare_require(source: &str, node: &Node) -> Option<ImportStatement> {
    let require_call = parse_lua_require_call(source, node)?;
    build_lua_import_statement(source, node, require_call, None)
}

fn parse_lua_local_require(source: &str, node: &Node) -> Option<ImportStatement> {
    let assignment = find_direct_child(node, "assignment_statement")?;
    let binding = extract_single_local_binding(source, &assignment)?;
    let require_call_node = extract_single_require_call(source, &assignment)?;
    let require_call = parse_lua_require_call(source, &require_call_node)?;
    build_lua_import_statement(source, node, require_call, Some(binding))
}

fn build_lua_import_statement(
    source: &str,
    statement_node: &Node,
    require_call: LuaRequireCall,
    binding: Option<String>,
) -> Option<ImportStatement> {
    if require_call.module_path.is_empty() {
        return None;
    }

    let raw_text = source[statement_node.byte_range()].to_string();
    let byte_range = statement_node.byte_range();
    let group = classify_group_lua(&require_call.module_path);
    let modifiers = match require_call.argument_style {
        RequireArgumentStyle::Parenthesized => Vec::new(),
        RequireArgumentStyle::BareString => vec![BARE_STRING_ARGUMENT_MODIFIER.to_string()],
    };
    let kind = if binding.is_some() {
        ImportKind::Value
    } else {
        ImportKind::SideEffect
    };

    Some(ImportStatement {
        module_path: require_call.module_path,
        names: Vec::new(),
        default_import: binding.clone(),
        namespace_import: None,
        kind,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: Vec::new(),
            namespace: None,
            alias: binding,
            modifiers,
            import_kind: None,
        },
    })
}

fn parse_lua_require_call(source: &str, node: &Node) -> Option<LuaRequireCall> {
    if node.kind() != "function_call" {
        return None;
    }

    let callee = find_direct_child(node, "identifier")?;
    if source[callee.byte_range()].trim() != "require" {
        return None;
    }

    let arguments = find_direct_child(node, "arguments")?;
    let module_path = extract_lua_string_argument(source, &arguments)?;
    let argument_text = source[arguments.byte_range()].trim_start();
    let argument_style = if argument_text.starts_with('(') {
        RequireArgumentStyle::Parenthesized
    } else {
        RequireArgumentStyle::BareString
    };

    Some(LuaRequireCall {
        module_path,
        argument_style,
    })
}

fn extract_single_local_binding(source: &str, assignment: &Node) -> Option<String> {
    let variable_list = find_direct_child(assignment, "variable_list")?;
    let identifiers = direct_children_text(source, &variable_list, &["identifier", "variable"]);
    if identifiers.len() == 1 {
        identifiers.into_iter().next()
    } else {
        None
    }
}

fn extract_single_require_call<'tree>(
    source: &str,
    assignment: &Node<'tree>,
) -> Option<Node<'tree>> {
    let expression_list = find_direct_child(assignment, "expression_list")?;
    let mut cursor = expression_list.walk();
    let mut expressions = expression_list.named_children(&mut cursor);
    let require_call = expressions.next()?;
    if expressions.next().is_some() {
        return None;
    }

    if require_call.kind() == "function_call"
        && parse_lua_require_call(source, &require_call).is_some()
    {
        Some(require_call)
    } else {
        None
    }
}

fn extract_lua_string_argument(source: &str, arguments: &Node) -> Option<String> {
    let string_node = find_direct_child(arguments, "string")?;
    Some(strip_lua_string_delimiters(
        source[string_node.byte_range()].trim(),
    ))
}

fn strip_lua_string_delimiters(raw: &str) -> String {
    if raw.len() >= 2 {
        let first = raw.as_bytes()[0];
        let last = raw.as_bytes()[raw.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return raw[1..raw.len() - 1].to_string();
        }
    }

    if raw.starts_with("[[") && raw.ends_with("]]") && raw.len() >= 4 {
        return raw[2..raw.len() - 2].to_string();
    }

    raw.to_string()
}

fn direct_children_text(source: &str, node: &Node, kinds: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if kinds.contains(&child.kind()) {
                let text = source[child.byte_range()].trim();
                if !text.is_empty() {
                    values.push(text.to_string());
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    values
}

fn find_direct_child<'tree>(node: &Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

pub fn generate_lua_import_line(req: &ImportRequest) -> String {
    let module_path = escape_lua_double_quoted_string(req.module_path);
    let use_bare_string_argument = req
        .modifiers
        .iter()
        .any(|modifier| modifier == BARE_STRING_ARGUMENT_MODIFIER);
    let require_expr = if use_bare_string_argument {
        format!("require \"{module_path}\"")
    } else {
        format!("require(\"{module_path}\")")
    };

    if let Some(binding) = req
        .default_import
        .or(req.alias)
        .filter(|binding| !binding.is_empty())
    {
        format!("local {binding} = {require_expr}")
    } else {
        require_expr
    }
}

fn escape_lua_double_quoted_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub struct LuaSyntax;

impl ImportSyntax for LuaSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_lua_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_lua_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_lua(module_path)
    }
}

pub static LUA_SYNTAX: LuaSyntax = LuaSyntax;
