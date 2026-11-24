pub use aft_parse::extract::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use crate::indent::IndentStyle;
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::{node_text, LangId};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashSet;
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::{Node, Tree};

#[cfg(test)]
mod tests {
    #[test]
    fn grammar_refactor_body_failures_are_not_silently_unchanged_or_conflict_free() {
        let source = "local function f() return 1 end";
        let tree = crate::parser::FileParser::parse_source(
            std::path::Path::new("f.lua"),
            source,
            crate::parser::LangId::Lua,
        )
        .unwrap();
        let conflict = super::detect_scope_conflicts(
            source,
            &tree,
            0,
            &[],
            source,
            crate::parser::LangId::Luau,
        )
        .unwrap_err();
        assert!(matches!(
            conflict,
            crate::error::AftError::GrammarUnavailable { .. }
        ));
        let substitute = super::substitute_params(
            source,
            &std::collections::HashMap::new(),
            crate::parser::LangId::Luau,
        )
        .unwrap_err();
        assert!(matches!(
            substitute,
            crate::error::AftError::GrammarUnavailable { .. }
        ));
    }
    use super::*;
    use crate::parser::grammar_for;
    use std::path::PathBuf;
    use tree_sitter::Parser;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(crate::test_support::manifest_dir())
            .join("tests/fixtures/extract_function")
            .join(name)
    }

    fn parse_source(source: &str, lang: LangId) -> Tree {
        let grammar = grammar_for(lang).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        parser.parse(source.as_bytes(), None).unwrap()
    }

    // --- Free variable detection: simple identifiers ---

    #[test]
    fn free_vars_detects_enclosing_function_params() {
        // `items` and `prefix` are function params → should be detected as free variables
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // Lines 5-8 (0-indexed): the body of processData that uses `items` and `prefix`
        // "  const filtered = items.filter(item => item.length > 0);"
        // "  const mapped = filtered.map(item => prefix + item);"
        // These lines reference `items` and `prefix` from the function params.
        let line5_start = crate::edit::line_col_to_byte(&source, 5, 0);
        let line6_end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result =
            detect_free_variables(&source, &tree, line5_start, line6_end, LangId::TypeScript);
        assert!(
            result.parameters.contains(&"items".to_string()),
            "should detect 'items' as parameter, got: {:?}",
            result.parameters
        );
        assert!(
            result.parameters.contains(&"prefix".to_string()),
            "should detect 'prefix' as parameter, got: {:?}",
            result.parameters
        );
        assert!(!result.has_this_or_self);
    }

    // --- Property access filtering ---

    #[test]
    fn free_vars_filters_property_identifiers() {
        // In `items.filter(...)`, `filter` should NOT be a free variable.
        // In `item.length`, `length` should NOT be a free variable.
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        let line5_start = crate::edit::line_col_to_byte(&source, 5, 0);
        let line6_end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result =
            detect_free_variables(&source, &tree, line5_start, line6_end, LangId::TypeScript);
        // "filter", "map", "length" should NOT appear
        assert!(
            !result.parameters.contains(&"filter".to_string()),
            "property 'filter' should not be a free variable"
        );
        assert!(
            !result.parameters.contains(&"length".to_string()),
            "property 'length' should not be a free variable"
        );
        assert!(
            !result.parameters.contains(&"map".to_string()),
            "property 'map' should not be a free variable"
        );
    }

    // --- Module-level vs function-level classification ---

    #[test]
    fn free_vars_skips_module_level_refs() {
        // `BASE_URL` is module-level → should NOT be a parameter
        // `console` is a global → should NOT be a parameter
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // processData body: lines 5-9
        let start = crate::edit::line_col_to_byte(&source, 5, 0);
        let end = crate::edit::line_col_to_byte(&source, 10, 0);

        let result = detect_free_variables(&source, &tree, start, end, LangId::TypeScript);
        assert!(
            !result.parameters.contains(&"BASE_URL".to_string()),
            "module-level 'BASE_URL' should not be a parameter, got: {:?}",
            result.parameters
        );
        assert!(
            !result.parameters.contains(&"console".to_string()),
            "'console' should not be a parameter, got: {:?}",
            result.parameters
        );
    }

    #[test]
    fn free_vars_plain_lexical_declaration_uses_real_enclosing_function() {
        let source = "function f(a: number) {\n  const x = a + 1;\n  return x;\n}\n";
        let tree = parse_source(source, LangId::TypeScript);
        let start = crate::edit::line_col_to_byte(source, 1, 0);
        let end = crate::edit::line_col_to_byte(source, 2, 0);

        let result = detect_free_variables(source, &tree, start, end, LangId::TypeScript);
        assert!(
            result.parameters.contains(&"a".to_string()),
            "plain const declaration should not stop enclosing-function lookup: {:?}",
            result.parameters
        );
    }

    // --- this/self detection ---

    #[test]
    fn free_vars_detects_this_in_ts() {
        let source = std::fs::read_to_string(fixture_path("sample_this.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // getUser method body lines 4-6 contain `this.users.get(key)`
        let start = crate::edit::line_col_to_byte(&source, 4, 0);
        let end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result = detect_free_variables(&source, &tree, start, end, LangId::TypeScript);
        assert!(result.has_this_or_self, "should detect 'this' reference");
    }

    #[test]
    fn free_vars_detects_self_in_python() {
        let source = r#"
class UserService:
    def get_user(self, id):
        key = id.lower()
        user = self.users.get(key)
        return user
"#;
        let tree = parse_source(source, LangId::Python);

        // Lines 3-4 (0-indexed) contain `self.users.get(key)`
        let start = crate::edit::line_col_to_byte(source, 4, 0);
        let end = crate::edit::line_col_to_byte(source, 5, 0);

        let result = detect_free_variables(source, &tree, start, end, LangId::Python);
        assert!(result.has_this_or_self, "should detect 'self' reference");
    }

    // --- Return value detection ---

    #[test]
    fn return_value_explicit_return() {
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // simpleHelper: lines 13-16 — contains "return added;"
        let start = crate::edit::line_col_to_byte(&source, 14, 0);
        let end = crate::edit::line_col_to_byte(&source, 17, 0);

        let result =
            detect_return_value(&source, &tree, start, end, None, LangId::TypeScript).unwrap();
        assert_eq!(result, ReturnKind::Expression("added".to_string()));
    }

    #[test]
    fn return_value_post_range_usage() {
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // processData lines 5-7: declares `filtered`, `mapped`
        // Lines 7-9 use `result` (which comes after mapped), but line 7 declares `result`
        // Let's extract lines 5-6 only: `filtered` and `mapped` are declared
        // and `filtered` is used on line 6, `mapped` is used on line 7
        let start = crate::edit::line_col_to_byte(&source, 5, 0);
        let end = crate::edit::line_col_to_byte(&source, 6, 0);

        // Enclosing function ends around line 10
        let fn_end = crate::edit::line_col_to_byte(&source, 10, 0);

        let result =
            detect_return_value(&source, &tree, start, end, Some(fn_end), LangId::TypeScript)
                .unwrap();
        // `filtered` is declared in-range and used after the range
        assert_eq!(result, ReturnKind::Variable("filtered".to_string()));
    }

    #[test]
    fn return_value_void() {
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // voidWork lines 20-21: no return, `greeting` is only used within
        let start = crate::edit::line_col_to_byte(&source, 20, 0);
        let end = crate::edit::line_col_to_byte(&source, 22, 0);

        let result = detect_return_value(
            &source,
            &tree,
            start,
            end,
            Some(crate::edit::line_col_to_byte(&source, 23, 0)),
            LangId::TypeScript,
        )
        .unwrap();
        assert_eq!(result, ReturnKind::Void);
    }

    #[test]
    fn return_value_collects_multiple_const_live_outs_in_order() {
        let source = "function f() {\n  const a = 1;\n  const b = 2;\n  return a + b;\n}\n";
        let tree = parse_source(source, LangId::TypeScript);
        let start = crate::edit::line_col_to_byte(source, 1, 0);
        let end = crate::edit::line_col_to_byte(source, 3, 0);

        let result =
            detect_return_value(source, &tree, start, end, None, LangId::TypeScript).unwrap();

        assert_eq!(
            result,
            ReturnKind::Variables(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            generate_extracted_function(
                "makeValues",
                &[],
                &result,
                "  const a = 1;\n  const b = 2;",
                "",
                LangId::TypeScript,
                IndentStyle::Spaces(2),
                "\n",
            ),
            "function makeValues() {\n  const a = 1;\n  const b = 2;\n  return { a, b };\n}"
        );
        assert_eq!(
            generate_call_site("makeValues", &[], &result, "  ", LangId::TypeScript),
            "  const { a, b } = makeValues();"
        );
    }

    #[test]
    fn return_value_generates_mutable_destructure_for_let_pair() {
        let source = "function f() {\n  let a = 1;\n  let b = 2;\n  return a + b;\n}\n";
        let tree = parse_source(source, LangId::TypeScript);
        let start = crate::edit::line_col_to_byte(source, 1, 0);
        let end = crate::edit::line_col_to_byte(source, 3, 0);

        let result =
            detect_return_value(source, &tree, start, end, None, LangId::TypeScript).unwrap();

        assert_eq!(
            result,
            ReturnKind::Variables(vec!["let a".to_string(), "let b".to_string()])
        );
        assert_eq!(
            generate_call_site("makeValues", &[], &result, "  ", LangId::TypeScript),
            "  let { a, b } = makeValues();"
        );
    }

    #[test]
    fn return_value_generates_python_tuple_pair() {
        let source = "def f():\n    a = 1\n    b = 2\n    return a + b\n";
        let tree = parse_source(source, LangId::Python);
        let start = crate::edit::line_col_to_byte(source, 1, 0);
        let end = crate::edit::line_col_to_byte(source, 3, 0);

        let result = detect_return_value(source, &tree, start, end, None, LangId::Python).unwrap();

        assert_eq!(
            generate_extracted_function(
                "make_values",
                &[],
                &result,
                "    a = 1\n    b = 2",
                "",
                LangId::Python,
                IndentStyle::Spaces(4),
                "\n",
            ),
            "def make_values():\n    a = 1\n    b = 2\n    return a, b"
        );
        assert_eq!(
            generate_call_site("make_values", &[], &result, "    ", LangId::Python),
            "    a, b = make_values()"
        );
    }

    #[test]
    fn return_value_rejects_mixed_javascript_binding_kinds() {
        let source = "function f() {\n  const a = 1;\n  let b = 2;\n  return a + b;\n}\n";
        let tree = parse_source(source, LangId::TypeScript);
        let start = crate::edit::line_col_to_byte(source, 1, 0);
        let end = crate::edit::line_col_to_byte(source, 3, 0);

        let error =
            detect_return_value(source, &tree, start, end, None, LangId::TypeScript).unwrap_err();

        assert!(error.message.contains("a (const)"), "{}", error.message);
        assert!(error.message.contains("b (let)"), "{}", error.message);
    }

    #[test]
    fn return_value_rejects_destructured_live_out_binding() {
        let source = "function f(input) {\n  const { a, b } = input;\n  return a + b;\n}\n";
        let tree = parse_source(source, LangId::TypeScript);
        let start = crate::edit::line_col_to_byte(source, 1, 0);
        let end = crate::edit::line_col_to_byte(source, 2, 0);

        let error =
            detect_return_value(source, &tree, start, end, None, LangId::TypeScript).unwrap_err();

        assert!(error.message.contains("unsupported live-out binding shape"));
        assert!(error.message.contains("a, b"), "{}", error.message);
    }

    #[test]
    fn single_live_out_generation_remains_byte_identical() {
        let return_kind = ReturnKind::Variable("let result".to_string());
        let extracted = generate_extracted_function(
            "computeInitial",
            &[],
            &return_kind,
            "  let result = compute();",
            "",
            LangId::TypeScript,
            IndentStyle::Spaces(2),
            "\n",
        );
        let call = generate_call_site(
            "computeInitial",
            &[],
            &return_kind,
            "  ",
            LangId::TypeScript,
        );

        assert_eq!(
            format!("{extracted}\n\nfunction f() {{\n{call}\n  return result;\n}}\n"),
            "function computeInitial() {\n  let result = compute();\n  return result;\n}\n\nfunction f() {\n  let result = computeInitial();\n  return result;\n}\n"
        );
    }

    // --- Function generation ---

    #[test]
    fn generate_ts_function_with_params() {
        let body = "const doubled = x * 2;\nconst added = doubled + 10;";
        let result = generate_extracted_function(
            "compute",
            &["x".to_string()],
            &ReturnKind::Variable("added".to_string()),
            body,
            "",
            LangId::TypeScript,
            IndentStyle::Spaces(2),
            "\n",
        );
        assert!(result.contains("function compute(x)"));
        assert!(result.contains("return added;"));
        assert!(result.contains("}"));
    }

    #[test]
    fn generate_ts_function_preserves_relative_indentation() {
        let body = "  for (const item of items) {\n    if (item.active) {\n      console.log(item.name);\n    }\n  }";
        let result = generate_extracted_function(
            "processItems",
            &["items".to_string()],
            &ReturnKind::Void,
            body,
            "",
            LangId::TypeScript,
            IndentStyle::Spaces(2),
            "\n",
        );
        assert_eq!(
            result,
            "function processItems(items) {\n  for (const item of items) {\n    if (item.active) {\n      console.log(item.name);\n    }\n  }\n}"
        );
    }

    #[test]
    fn generate_py_function_with_params() {
        let body = "doubled = x * 2\nadded = doubled + 10";
        let result = generate_extracted_function(
            "compute",
            &["x".to_string()],
            &ReturnKind::Variable("added".to_string()),
            body,
            "",
            LangId::Python,
            IndentStyle::Spaces(4),
            "\n",
        );
        assert!(result.contains("def compute(x):"));
        assert!(result.contains("return added"));
    }

    #[test]
    fn generate_call_site_with_return_var() {
        let call = generate_call_site(
            "compute",
            &["x".to_string()],
            &ReturnKind::Variable("result".to_string()),
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  const result = compute(x);");
    }

    #[test]
    fn generate_call_site_preserves_let_return_var() {
        let call = generate_call_site(
            "compute",
            &[],
            &ReturnKind::Variable("let result".to_string()),
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  let result = compute();");
    }

    #[test]
    fn generate_call_site_void() {
        let call = generate_call_site(
            "doWork",
            &["a".to_string(), "b".to_string()],
            &ReturnKind::Void,
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  doWork(a, b);");
    }

    #[test]
    fn generate_call_site_return_expression() {
        let call = generate_call_site(
            "compute",
            &["x".to_string()],
            &ReturnKind::Expression("x * 2".to_string()),
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  return compute(x);");
    }

    // --- Python free variables ---

    #[test]
    fn free_vars_python_function_params() {
        let source = std::fs::read_to_string(fixture_path("sample.py")).unwrap();
        let tree = parse_source(&source, LangId::Python);

        // process_data body: lines 5-8 reference `items` and `prefix`
        let start = crate::edit::line_col_to_byte(&source, 5, 0);
        let end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result = detect_free_variables(&source, &tree, start, end, LangId::Python);
        assert!(
            result.parameters.contains(&"items".to_string()),
            "should detect 'items': {:?}",
            result.parameters
        );
        assert!(
            result.parameters.contains(&"prefix".to_string()),
            "should detect 'prefix': {:?}",
            result.parameters
        );
        assert!(!result.has_this_or_self);
    }

    // --- validate_single_return ---

    #[test]
    fn validate_single_return_single() {
        let source =
            "function add(a: number, b: number): number {\n  const sum = a + b;\n  return sum;\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        let fn_node = root.child(0).unwrap(); // function_declaration
        assert!(validate_single_return(source, &tree, &fn_node, LangId::TypeScript).is_ok());
    }

    #[test]
    fn validate_single_return_void() {
        let source = "function greet(name: string): void {\n  console.log(name);\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        let fn_node = root.child(0).unwrap();
        assert!(validate_single_return(source, &tree, &fn_node, LangId::TypeScript).is_ok());
    }

    #[test]
    fn validate_single_return_expression_body() {
        let source = "const double = (n: number): number => n * 2;";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        // lexical_declaration > variable_declarator > arrow_function
        let lex_decl = root.child(0).unwrap();
        let var_decl = lex_decl.child(1).unwrap(); // variable_declarator
        let arrow = var_decl.child_by_field_name("value").unwrap();
        assert_eq!(arrow.kind(), "arrow_function");
        assert!(validate_single_return(source, &tree, &arrow, LangId::TypeScript).is_ok());
    }

    #[test]
    fn validate_single_return_multiple() {
        let source = "function abs(x: number): number {\n  if (x > 0) {\n    return x;\n  }\n  return -x;\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        let fn_node = root.child(0).unwrap();
        let result = validate_single_return(source, &tree, &fn_node, LangId::TypeScript);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 2);
    }

    // --- detect_scope_conflicts ---

    #[test]
    fn scope_conflicts_none() {
        // No overlap between call site scope and body vars
        let source = "function main() {\n  const x = 10;\n  const y = add(x, 5);\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let body_text = "const sum = a + b;";
        let call_byte = crate::edit::line_col_to_byte(source, 2, 0);
        let conflicts =
            detect_scope_conflicts(source, &tree, call_byte, &[], body_text, LangId::TypeScript)
                .unwrap();
        assert!(
            conflicts.is_empty(),
            "expected no conflicts, got: {:?}",
            conflicts
        );
    }

    #[test]
    fn scope_conflicts_detected() {
        // `temp` exists at call site and inside body
        let source = "function main() {\n  const temp = 99;\n  const result = compute(5);\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let body_text = "const temp = x * 2;\nconst result2 = temp + 10;";
        let call_byte = crate::edit::line_col_to_byte(source, 2, 0);
        let conflicts =
            detect_scope_conflicts(source, &tree, call_byte, &[], body_text, LangId::TypeScript)
                .unwrap();
        assert!(!conflicts.is_empty(), "expected conflict for 'temp'");
        assert!(
            conflicts.iter().any(|c| c.name == "temp"),
            "conflicts: {:?}",
            conflicts
        );
        assert!(
            conflicts.iter().any(|c| c.suggested == "temp_inlined"),
            "should suggest temp_inlined"
        );
    }

    // --- substitute_params ---

    #[test]
    fn substitute_params_basic() {
        let body = "const sum = a + b;";
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), "x".to_string());
        map.insert("b".to_string(), "y".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript).unwrap();
        assert_eq!(result, "const sum = x + y;");
    }

    #[test]
    fn substitute_params_whole_word() {
        // Should NOT replace `i` inside `items`
        let body = "const result = items.filter(i => i > 0);";
        let mut map = std::collections::HashMap::new();
        map.insert("i".to_string(), "index".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript).unwrap();
        // `items` should be untouched, and the nested arrow's shadowing `i`
        // parameter/reference pair should also be left alone.
        assert_eq!(result, body);
    }

    #[test]
    fn substitute_params_rewrites_outer_reference_not_shadowed_arrow_param() {
        let body = "return x + items.map(x => x + 1)[0];";
        let mut map = std::collections::HashMap::new();
        map.insert("x".to_string(), "5".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript).unwrap();
        assert_eq!(result, "return 5 + items.map(x => x + 1)[0];");
    }

    #[test]
    fn substitute_params_noop_same_name() {
        let body = "const sum = x + y;";
        let mut map = std::collections::HashMap::new();
        map.insert("x".to_string(), "x".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript).unwrap();
        assert_eq!(result, "const sum = x + y;");
    }

    #[test]
    fn substitute_params_empty_map() {
        let body = "const sum = a + b;";
        let map = std::collections::HashMap::new();
        let result = substitute_params(body, &map, LangId::TypeScript).unwrap();
        assert_eq!(result, body);
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;
    use std::path::Path;

    fn free_variables(source: &str, selected: &str, lang: LangId) -> Vec<String> {
        let path = Path::new(if lang == LangId::Python {
            "a.py"
        } else {
            "a.ts"
        });
        let tree = crate::parser::parse_source_with_cached_parser(path, source, lang).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let start = source.find(selected).unwrap();
        detect_free_variables(source, &tree, start, start + selected.len(), lang).parameters
    }

    #[test]
    fn python_typed_default_and_splat_parameters_are_bindings() {
        let source = "def outer(a: Kind, b=default_value, *args: Kind, c: Kind=other, **kwargs: Kind):\r\n    consume(a, b, args, c, kwargs, Kind, default_value, other)\r\n";
        let selected = "consume(a, b, args, c, kwargs, Kind, default_value, other)";
        let params = free_variables(source, selected, LangId::Python);
        assert_eq!(params, ["a", "b", "args", "c", "kwargs"]);
        let generated = generate_extracted_function(
            "helper",
            &params,
            &ReturnKind::Void,
            "    consume(a, b, args, c, kwargs)",
            "",
            LangId::Python,
            IndentStyle::Spaces(4),
            "\n",
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.py");
        let config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        crate::edit::write_format_validate(
            &crate::edit::test_launcher::Unconfined,
            &path,
            &generated,
            &config,
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"def helper(a, b, args, c, kwargs):\n    consume(a, b, args, c, kwargs)"
        );
    }

    #[test]
    fn destructuring_and_scoped_local_binders_are_parameters() {
        for (source, selected, lang, expected) in [
            ("function outer(input) { const {key: renamed, other, ...rest} = input; consume(renamed, other, rest); }", "consume(renamed, other, rest)", LangId::TypeScript, vec!["renamed", "other", "rest"]),
            ("function outer(xs) { for (const [a,b] of xs) { consume(a, b); } }", "consume(a, b)", LangId::TypeScript, vec!["a", "b"]),
            ("function outer(xs) { for (var item of xs) {} consume(item); }", "consume(item)", LangId::TypeScript, vec!["item"]),
            ("let item; function outer(xs) { for (item of xs) { consume(item); } }", "consume(item)", LangId::TypeScript, vec![]),
            ("function outer() { const value = function self() { consume(self); }; }", "consume(self)", LangId::TypeScript, vec!["self"]),
            ("function outer() { try {} catch ({message}) { consume(message); } }", "consume(message)", LangId::TypeScript, vec!["message"]),
            ("function outer() { function helper() {} consume(helper); }", "consume(helper)", LangId::TypeScript, vec!["helper"]),
            ("def outer(xs):\n    a, b = xs\n    consume(a, b)\n", "consume(a, b)", LangId::Python, vec!["a", "b"]),
            ("def outer(xs):\n    for a, b in xs:\n        consume(a, b)\n", "consume(a, b)", LangId::Python, vec!["a", "b"]),
            ("def outer():\n    with open('x') as handle:\n        consume(handle)\n", "consume(handle)", LangId::Python, vec!["handle"]),
            ("def outer():\n    try:\n        work()\n    except Error as error:\n        consume(error)\n", "consume(error)", LangId::Python, vec!["error"]),
            ("function outer() { function nested() { const hidden = 1; } consume(hidden); }", "consume(hidden)", LangId::TypeScript, vec![]),
            ("function outer() { { let hidden = 1; } consume(hidden); }", "consume(hidden)", LangId::TypeScript, vec![]),
        ] {
            assert_eq!(free_variables(source, selected, lang), expected, "{source}");
        }
        let source = "function outer(input) { const {key: renamed, other, ...rest} = input; consume(renamed, other, rest); }";
        let params = free_variables(source, "consume(renamed, other, rest)", LangId::TypeScript);
        let generated = generate_extracted_function(
            "helper",
            &params,
            &ReturnKind::Void,
            "    consume(renamed, other, rest);",
            "",
            LangId::TypeScript,
            IndentStyle::Spaces(4),
            "\n",
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        crate::edit::write_format_validate(
            &crate::edit::test_launcher::Unconfined,
            &path,
            &generated,
            &config,
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"function helper(renamed, other, rest) {\n    consume(renamed, other, rest);\n}"
        );
    }

    #[test]
    fn multiline_literal_interiors_and_crlf_survive_reindentation() {
        for (body, lang, expected) in [
            ("    const value = `first\r\n  literal\r\n\r\nlast`;\r\n    consume(value);", LangId::TypeScript, "function helper() {\r\n    const value = `first\r\n  literal\r\n\r\nlast`;\r\n    consume(value);\r\n}"),
            ("    value = '''first\r\n  literal\r\n\r\nlast'''\r\n    consume(value)", LangId::Python, "def helper():\r\n    value = '''first\r\n  literal\r\n\r\nlast'''\r\n    consume(value)"),
        ] {
            let generated = generate_extracted_function("helper", &[], &ReturnKind::Void, body, "", lang, IndentStyle::Spaces(4), "\r\n");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("out.txt");
            let config = crate::config::Config { format_on_edit: false, ..Default::default() };
            crate::edit::write_format_validate(&crate::edit::test_launcher::Unconfined, &path, &generated, &config, &serde_json::json!({})).unwrap();
            assert_eq!(std::fs::read(path).unwrap(), expected.as_bytes());
        }
    }
}

#[cfg(test)]
mod approved_audit11 {
    use super::*;

    #[test]
    fn nested_functions_keep_their_own_returns_await_and_yield() {
        for (source, lang) in [
            ("function outer() {\n    const inner = function () { return; }; inner();\n    finish();\n}\n", LangId::JavaScript),
            ("function outer() {\n    const inner = async () => { await work(); return; }; inner();\n    finish();\n}\n", LangId::TypeScript),
            ("function outer() {\n    function* inner() { yield value; return; } inner();\n    finish();\n}\n", LangId::JavaScript),
            ("def outer():\n    async def inner():\n        await work()\n        return\n    finish()\n", LangId::Python),
            ("def outer():\n    inner = lambda: (yield value)\n    finish()\n", LangId::Python),
        ] {
            let tree = crate::parser::parse_source_with_cached_parser(std::path::Path::new("source"), source, lang).unwrap();
            assert!(!tree.root_node().has_error(), "{}", tree.root_node().to_sexp());
            let start = source.find('\n').unwrap() + 1;
            let end = source.find("    finish()").unwrap();
            assert!(unsupported_extraction_control_flow(&tree, start, end, lang).is_none(), "{source}");
            assert!(matches!(detect_return_value(source, &tree, start, end, None, lang).unwrap(), ReturnKind::Void), "{source}");
        }
    }
}
