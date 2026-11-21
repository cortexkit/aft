use crate::parser::LangId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonymizeAs {
    Variable,
    Field,
    None,
}

pub fn node_cost(lang: LangId, node_kind: &str) -> u32 {
    use LangId::*;
    match (lang, node_kind) {
        // Statements (cost 2)
        (
            Rust,
            "let_declaration"
            | "expression_statement"
            | "return_expression"
            | "if_expression"
            | "match_expression"
            | "for_expression"
            | "while_expression"
            | "loop_expression"
            | "break_expression"
            | "continue_expression",
        ) => 2,
        (
            TypeScript | Tsx | JavaScript,
            "variable_declaration"
            | "lexical_declaration"
            | "expression_statement"
            | "return_statement"
            | "if_statement"
            | "switch_statement"
            | "for_statement"
            | "while_statement"
            | "do_statement"
            | "break_statement"
            | "continue_statement"
            | "throw_statement",
        ) => 2,
        (
            Python,
            "expression_statement"
            | "if_statement"
            | "for_statement"
            | "while_statement"
            | "return_statement"
            | "raise_statement"
            | "assert_statement"
            | "break_statement"
            | "continue_statement",
        ) => 2,
        (
            Go,
            "short_var_declaration"
            | "var_declaration"
            | "expression_statement"
            | "if_statement"
            | "for_statement"
            | "switch_statement"
            | "return_statement"
            | "break_statement"
            | "continue_statement",
        ) => 2,

        // Identifiers (cost 1)
        (
            _,
            "identifier"
            | "property_identifier"
            | "field_identifier"
            | "type_identifier"
            | "shorthand_property_identifier",
        ) => 1,

        // Literals per language (cost 1)
        (
            Rust,
            "integer_literal" | "float_literal" | "string_literal" | "raw_string_literal"
            | "char_literal" | "boolean_literal",
        ) => 1,
        (
            TypeScript | Tsx | JavaScript,
            "number"
            | "string"
            | "string_fragment"
            | "template_string"
            | "template_substitution"
            | "regex"
            | "true"
            | "false"
            | "null"
            | "undefined",
        ) => 1,
        (
            Python,
            "integer"
            | "float"
            | "string"
            | "string_content"
            | "concatenated_string"
            | "true"
            | "false"
            | "none",
        ) => 1,
        (
            Go,
            "int_literal"
            | "float_literal"
            | "imaginary_literal"
            | "rune_literal"
            | "interpreted_string_literal"
            | "raw_string_literal"
            | "true"
            | "false"
            | "nil",
        ) => 1,

        // Expression-shaped nodes (cost 1)
        (
            _,
            "binary_expression"
            | "unary_expression"
            | "call_expression"
            | "call"
            | "method_invocation"
            | "member_expression"
            | "field_expression"
            | "index_expression"
            | "subscript_expression",
        ) => 1,

        // Block-like containers and trivia (cost 0)
        _ => 0,
    }
}

pub fn is_anonymizable(node_kind: &str) -> AnonymizeAs {
    match node_kind {
        "identifier" => AnonymizeAs::Variable,
        "property_identifier" | "field_identifier" | "shorthand_property_identifier" => {
            AnonymizeAs::Field
        }
        _ => AnonymizeAs::None,
    }
}
