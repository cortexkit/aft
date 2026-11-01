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

pub(crate) fn classify_group_lua(_module_path: &str) -> ImportGroup {
    ImportGroup::External
}

pub(crate) fn parse_lua_imports(source: &str, tree: &Tree) -> ImportBlock {
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

pub(crate) fn generate_lua_import_line(req: &ImportRequest) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_lua(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Lua).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Lua);
        (tree, block)
    }

    /// Grammar fixture: lock the tree-sitter-lua node kinds this parser uses.
    /// The current grammar emits top-level `function_call` nodes for bare
    /// requires and `variable_declaration -> assignment_statement` for local
    /// bindings. The require argument lives under an `arguments` node as a
    /// direct `string` child in both parenthesized and bare-string forms.
    #[test]
    fn lua_grammar_node_kinds_are_stable() {
        let src = "local x = require(\"y\")\nrequire \"z\"\nrequire(\"w\")\nlocal bar = require(\"pkg.bar\")\n";
        let (tree, _) = parse_lua(src);
        let mut kinds = BTreeSet::new();

        fn walk(node: Node, kinds: &mut BTreeSet<String>) {
            kinds.insert(node.kind().to_string());
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    walk(cursor.node(), kinds);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        walk(tree.root_node(), &mut kinds);
        for required in [
            "chunk",
            "variable_declaration",
            "local",
            "assignment_statement",
            "variable_list",
            "expression_list",
            "function_call",
            "identifier",
            "arguments",
            "string",
            "string_content",
        ] {
            assert!(
                kinds.contains(required),
                "lua grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_lua_all_supported_forms() {
        let (_, block) = parse_lua(
            "local foo = require(\"foo\")\nlocal bar = require \"pkg.bar\"\nrequire(\"side.effect\")\nrequire \"boot\"\n",
        );
        assert_eq!(block.imports.len(), 4);

        assert_lua_import(
            &block.imports[0],
            "foo",
            Some("foo"),
            ImportKind::Value,
            &[],
        );
        assert_lua_import(
            &block.imports[1],
            "pkg.bar",
            Some("bar"),
            ImportKind::Value,
            &[BARE_STRING_ARGUMENT_MODIFIER],
        );
        assert_lua_import(
            &block.imports[2],
            "side.effect",
            None,
            ImportKind::SideEffect,
            &[],
        );
        assert_lua_import(
            &block.imports[3],
            "boot",
            None,
            ImportKind::SideEffect,
            &[BARE_STRING_ARGUMENT_MODIFIER],
        );
    }

    fn assert_lua_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_binding: Option<&str>,
        expected_kind: ImportKind,
        expected_modifiers: &[&str],
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.default_import.as_deref(), expected_binding);
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, expected_kind);
        assert_eq!(imp.group, ImportGroup::External);

        assert_eq!(
            imp.form,
            ImportForm::Structured {
                named: vec![],
                namespace: None,
                alias: expected_binding.map(str::to_string),
                modifiers: expected_modifiers
                    .iter()
                    .map(|modifier| modifier.to_string())
                    .collect(),
                import_kind: None,
            }
        );
    }

    #[test]
    fn generate_lua_all_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest::legacy("foo", &[], Some("foo"), None, false)
            ),
            "local foo = require(\"foo\")"
        );
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest::legacy("side.effect", &[], None, None, false)
            ),
            "require(\"side.effect\")"
        );

        let bare_string_modifier = vec![BARE_STRING_ARGUMENT_MODIFIER.to_string()];
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest {
                    module_path: "pkg.bar",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("bar"),
                    type_only: false,
                    modifiers: &bare_string_modifier,
                    import_kind: None,
                }
            ),
            "local bar = require \"pkg.bar\""
        );
        assert_eq!(
            generate_import(
                LangId::Lua,
                &ImportRequest {
                    module_path: "boot",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &bare_string_modifier,
                    import_kind: None,
                }
            ),
            "require \"boot\""
        );
    }

    #[test]
    fn parse_lua_requires_one_local_binding_and_one_rhs_expression() {
        let (_, block) = parse_lua(
            "local pure = require(\"pure\")\n\
             local mixed = require(\"mixed\"), keep()\n\
             local two_requires = require(\"first\"), require(\"second\")\n\
             local first, second = require(\"first\"), require(\"second\")\n\
             plain = require(\"plain\")\n",
        );

        let modules: Vec<&str> = block
            .imports
            .iter()
            .map(|import| import.module_path.as_str())
            .collect();
        assert_eq!(modules, vec!["pure"]);
    }

    #[test]
    fn classify_group_lua_always_external() {
        assert_eq!(classify_group_lua("foo"), ImportGroup::External);
        assert_eq!(classify_group_lua("pkg.bar"), ImportGroup::External);
        assert_eq!(classify_group_lua("./local"), ImportGroup::External);
    }

    #[test]
    fn lua_round_trips_through_parse_generate() {
        for src in [
            "local foo = require(\"foo\")",
            "local bar = require \"pkg.bar\"",
            "require(\"side.effect\")",
            "require \"boot\"",
        ] {
            let (_, block) = parse_lua(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (alias, modifiers) = match &imp.form {
                ImportForm::Structured {
                    alias, modifiers, ..
                } => (alias.as_deref(), modifiers.as_slice()),
                other => panic!("expected Lua Structured import, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Lua,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: imp.default_import.as_deref(),
                    namespace: None,
                    alias,
                    type_only: false,
                    modifiers,
                    import_kind: None,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
