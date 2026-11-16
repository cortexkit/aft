pub use aft_parse::calls::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::grammar_for;

    #[test]
    fn rust_value_binding_work_is_linear_and_results_are_identical() {
        let mut source = String::from("fn process() {\n");
        for i in 0..1000 {
            source.push_str(&format!(
                "let v{i} = callback; consume(v{i}); consume(callback);\n"
            ));
        }
        source.push_str("}\nfn scopes(callback: fn()) {\nlet later = callback;\n{ consume(other); let other = later; consume(other); }\nlet (a, b) = (later, ns::callback);\nconsume(a);\nfor other in values { consume(other); consume(callback); }\nif let Some(other) = option { consume(other); } else { consume(other); }\nwhile let Some(other) = option { consume(other); }\nmatch value { Some(other) => consume(other), _ => consume(callback) }\nlet f = |other| { consume(other); consume(callback); };\n}\n");
        let tree = parse_source(LangId::Rust, &source);
        assert!(!tree.root_node().has_error());
        VALUE_BINDING_WORK.with(|work| work.set(0));
        let reference = legacy_rust_value_references(&source, tree.root_node());
        let before = VALUE_BINDING_WORK.with(|work| work.get());
        VALUE_BINDING_WORK.with(|work| work.set(0));
        let actual = extract_rust_value_references(&source, tree.root_node());
        let after = VALUE_BINDING_WORK.with(|work| work.get());
        assert_eq!(actual, reference);
        assert!(actual.len() >= 2000);
        eprintln!("Rust binding work {before} -> {after}");
        assert!(
            after <= 10000,
            "{after} block binding operations for 1000 declarations"
        );
    }

    #[test]
    fn non_name_callee_expressions_are_not_recorded_as_call_names() {
        let source = "(function () { inner(); })();\n(() => 1)();\nfactory()();\nnamed();\n";
        let tree = parse_source(LangId::TypeScript, source);
        let names = extract_calls_full(
            source,
            tree.root_node(),
            0,
            source.len(),
            LangId::TypeScript,
        )
        .into_iter()
        .map(|call| call.1)
        .collect::<Vec<_>>();
        assert_eq!(names, vec!["inner", "factory", "named"]);

        // Go selector expressions use the text fallback but still name a callee.
        let source = "package main\nfunc main() { fmt.Println(1); helper(); }\n";
        let tree = parse_source(LangId::Go, source);
        let names = extract_calls_full(source, tree.root_node(), 0, source.len(), LangId::Go)
            .into_iter()
            .map(|call| call.1)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Println", "helper"]);
    }

    #[test]
    fn computed_member_literal_rejects_a_single_quote_without_panicking() {
        assert_eq!(computed_member_literal("\""), None);
        assert_eq!(computed_member_literal("'"), None);
        assert_eq!(computed_member_literal("\"\""), Some(String::new()));
        assert_eq!(
            computed_member_literal("'method'"),
            Some("method".to_string())
        );
        assert_eq!(
            computed_member_literal("\"方法\""),
            Some("方法".to_string())
        );
    }

    #[test]
    fn rust_macro_fragments_reuse_one_parser_per_thread() {
        RUST_FRAGMENT_PARSER_INITIALIZATIONS.with(|count| count.set(0));
        let source = "fn main() { check!(first()); check!(second()); check!(nested!(third())); }";
        let tree = parse_source(LangId::Rust, source);
        let (calls, _) =
            extract_rust_calls_with_macro_facts(source, tree.root_node(), 0, source.len());
        for name in ["first", "second", "third"] {
            assert!(calls.iter().any(|call| call.0 == name));
        }
        RUST_FRAGMENT_PARSER_INITIALIZATIONS.with(|count| assert_eq!(count.get(), 1));
    }

    #[test]
    fn rust_macro_fragment_cache_preserves_dead_code_fact_bytes() {
        use crate::parser::dead_code_facts::StoredFileFacts;
        let source = r#"
macro_rules! top { ($f:expr) => { wrap!(nested!($f())) }; }
#[attribute_macro(callback = "hook")]
fn entry() {
    macro_rules! unused { () => { never_called() }; }
    macro_rules! inner { () => { helper(); wrap!(nested!(callback())); }; }
    macro_rules! outer { () => { inner!(); }; }
    outer!();
    wrap!(nested!(callback()));
    fn nested_function() { wrap!(nested!(callback())); }
}
#[cfg(unix)]
fn alternative() { wrap!(unix::run()); }
#[cfg(windows)]
fn alternative() { wrap!(windows::run()); }
"#;
        let tree = parse_source(LangId::Rust, source);
        assert!(!tree.root_node().has_error());
        RUST_FRAGMENT_PARSES.with(|count| count.set(0));
        let reference_calls =
            extract_rust_calls_with_macro_facts(source, tree.root_node(), 0, source.len());
        let reference_bytes =
            StoredFileFacts::encode_for_record(source.as_bytes(), LangId::Rust, &tree);
        let uncached_parses = RUST_FRAGMENT_PARSES.with(|count| count.get());
        assert!(uncached_parses > 0);

        RUST_FRAGMENT_PARSES.with(|count| count.set(0));
        let cached_parses;
        {
            let _cache = cache_rust_macro_fragments();
            let calls =
                extract_rust_calls_with_macro_facts(source, tree.root_node(), 0, source.len());
            assert_eq!(calls.0, reference_calls.0);
            assert_eq!(calls.1.bodies, reference_calls.1.bodies);
            assert_eq!(
                calls.1.unparsed_mentions,
                reference_calls.1.unparsed_mentions
            );
            cached_parses = RUST_FRAGMENT_PARSES.with(|count| count.get());
            assert!(cached_parses > 0);
            assert_eq!(
                StoredFileFacts::encode_for_record(source.as_bytes(), LangId::Rust, &tree),
                reference_bytes
            );
            RUST_FRAGMENT_PARSES.with(|count| assert_eq!(count.get(), cached_parses));
        }
        assert!(cached_parses < uncached_parses);

        // Exercise the production scope too: sharing only inside this test
        // would preserve facts while leaving content extraction unchanged.
        RUST_FRAGMENT_PARSES.with(|count| count.set(0));
        let record = crate::data_layer::content::extract(source.as_bytes(), "rust").unwrap();
        let aft_store::content::RecordBody::Parsed { body, .. } = record.data else {
            panic!("expected a parsed Rust content record");
        };
        assert_eq!(body.dead_code.as_deref(), Some(reference_bytes.as_slice()));
        RUST_FRAGMENT_PARSES.with(|count| assert_eq!(count.get(), cached_parses));
    }

    #[test]
    fn rust_macro_fragment_cache_is_scoped_and_unwind_safe() {
        RUST_FRAGMENT_PARSES.with(|count| count.set(0));
        let fragment = "first()";
        let failed = std::panic::catch_unwind(|| {
            let _cache = cache_rust_macro_fragments();
            let first = parse_rust_macro_fragment(fragment).unwrap();
            {
                let _nested_cache = cache_rust_macro_fragments();
                let second = parse_rust_macro_fragment(fragment).unwrap();
                assert_eq!(first.root_node().to_sexp(), second.root_node().to_sexp());
            }
            assert!(parse_rust_macro_fragment(fragment).is_some());
            assert!(parse_rust_macro_fragment("second()").is_some());
            RUST_FRAGMENT_PARSES.with(|count| assert_eq!(count.get(), 2));
            panic!("exercise extraction unwinding");
        });
        assert!(failed.is_err());
        RUST_FRAGMENT_TREES.with(|trees| assert!(trees.borrow().is_none()));
        let _cache = cache_rust_macro_fragments();
        assert!(parse_rust_macro_fragment(fragment).is_some());
        RUST_FRAGMENT_PARSES.with(|count| assert_eq!(count.get(), 3));
    }

    fn parse_source(lang: LangId, source: &str) -> tree_sitter::Tree {
        let grammar = grammar_for(lang).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar)
            .expect("grammar should initialize");
        parser.parse(source, None).expect("parse source")
    }

    fn parse_typescript(source: &str) -> tree_sitter::Tree {
        parse_source(LangId::TypeScript, source)
    }

    fn deeply_nested_calls(depth: usize) -> String {
        let mut source = String::with_capacity(depth * 3 + 32);
        source.push_str("const x = ");
        for _ in 0..depth {
            source.push_str("f(");
        }
        source.push('a');
        for _ in 0..depth {
            source.push(')');
        }
        source.push_str(";\n");
        source
    }

    fn deeply_nested_type(depth: usize) -> String {
        let mut source = String::with_capacity(depth * 10 + 32);
        source.push_str("type T = ");
        for _ in 0..depth {
            source.push_str("Box<");
        }
        source.push('A');
        for _ in 0..depth {
            source.push('>');
        }
        source.push_str(";\n");
        source
    }

    fn extracted_call_pairs(lang: LangId, source: &str) -> Vec<(String, String)> {
        let tree = parse_source(lang, source);
        extract_calls_full(source, tree.root_node(), 0, source.len(), lang)
            .into_iter()
            .map(|(full, short, _, _, _)| (full, short))
            .collect()
    }

    fn assert_extracted_call(lang: LangId, source: &str, full: &str, short: &str) {
        let calls = extracted_call_pairs(lang, source);
        assert!(
            calls
                .iter()
                .any(|(actual_full, actual_short)| actual_full == full && actual_short == short),
            "expected {full:?}/{short:?} in {calls:?}"
        );
    }

    fn build_fixture_call_data(extension: &str, source: &str) -> crate::callgraph::FileCallData {
        let dir = tempfile::tempdir().expect("create temp fixture dir");
        let path = dir.path().join(format!("fixture.{extension}"));
        std::fs::write(&path, source).expect("write fixture");
        crate::callgraph::build_file_data(&path).expect("build fixture call data")
    }

    fn assert_symbol_has_call(
        data: &crate::callgraph::FileCallData,
        symbol: &str,
        full: &str,
        short: &str,
    ) {
        let calls = data
            .calls_by_symbol
            .iter()
            .find(|(name, _)| name.rsplit("::").next().is_some_and(|tail| tail == symbol))
            .map(|(_, calls)| calls)
            .unwrap_or_else(|| {
                panic!(
                    "expected calls for symbol {symbol:?}; available symbols: {:?}",
                    data.calls_by_symbol.keys().collect::<Vec<_>>()
                )
            });

        assert!(
            calls
                .iter()
                .any(|call| call.full_callee == full && call.callee_name == short),
            "expected {full:?}/{short:?} in calls for {symbol:?}: {calls:?}"
        );
    }

    #[test]
    fn extracts_c_calls_and_attributes_them_to_function_symbols() {
        let source = r#"
int foo(void);
struct Obj { int (*method)(void); };
void caller(struct Obj *p, struct Obj obj) {
    foo();
    obj.method();
    p->method();
}
"#;

        assert_eq!(call_node_kinds(LangId::C), vec!["call_expression"]);
        assert_extracted_call(LangId::C, source, "foo", "foo");
        assert_extracted_call(LangId::C, source, "obj.method", "method");
        assert_extracted_call(LangId::C, source, "p->method", "method");

        let data = build_fixture_call_data("c", source);
        assert_symbol_has_call(&data, "caller", "foo", "foo");
        assert_symbol_has_call(&data, "caller", "obj.method", "method");
        assert_symbol_has_call(&data, "caller", "p->method", "method");
    }

    #[test]
    fn extracts_cpp_calls_and_attributes_them_to_function_symbols() {
        let source = r#"
namespace Foo { void bar(); }
struct Painter { void draw(); };
void foo();
void caller(Painter *p, Painter obj) {
    foo();
    obj.draw();
    p->draw();
    Foo::bar();
    Foo::templ<int>();
}
"#;

        assert_eq!(call_node_kinds(LangId::Cpp), vec!["call_expression"]);
        assert_extracted_call(LangId::Cpp, source, "foo", "foo");
        assert_extracted_call(LangId::Cpp, source, "obj.draw", "draw");
        assert_extracted_call(LangId::Cpp, source, "p->draw", "draw");
        assert_extracted_call(LangId::Cpp, source, "Foo::bar", "bar");
        assert_extracted_call(LangId::Cpp, source, "Foo::templ<int>", "templ");

        let data = build_fixture_call_data("cpp", source);
        assert_symbol_has_call(&data, "caller", "foo", "foo");
        assert_symbol_has_call(&data, "caller", "p->draw", "draw");
        assert_symbol_has_call(&data, "caller", "Foo::bar", "bar");
    }

    #[test]
    fn extracts_csharp_calls_and_attributes_them_to_method_symbols() {
        let source = r#"
class Service { public void Find() {} }
class Program {
    void Foo() {}
    void Caller(Service svc) {
        Foo();
        svc.Find();
        Generic<int>();
    }
    T Generic<T>() => default;
}
"#;

        assert_eq!(
            call_node_kinds(LangId::CSharp),
            vec!["invocation_expression"]
        );
        assert_extracted_call(LangId::CSharp, source, "Foo", "Foo");
        assert_extracted_call(LangId::CSharp, source, "svc.Find", "Find");
        assert_extracted_call(LangId::CSharp, source, "Generic<int>", "Generic");

        let data = build_fixture_call_data("cs", source);
        assert_symbol_has_call(&data, "Caller", "Foo", "Foo");
        assert_symbol_has_call(&data, "Caller", "svc.Find", "Find");
        assert_symbol_has_call(&data, "Caller", "Generic<int>", "Generic");
    }

    #[test]
    fn extracts_zig_calls_and_attributes_them_to_function_symbols() {
        let source = r#"
fn foo() void {}
const Obj = struct {
    fn method(self: *Obj) void {}
};
fn caller(obj: *Obj) void {
    foo();
    obj.method();
    std.debug.print("x", .{});
}
"#;

        assert_eq!(call_node_kinds(LangId::Zig), vec!["call_expression"]);
        assert_extracted_call(LangId::Zig, source, "foo", "foo");
        assert_extracted_call(LangId::Zig, source, "obj.method", "method");
        assert_extracted_call(LangId::Zig, source, "std.debug.print", "print");

        let data = build_fixture_call_data("zig", source);
        assert_symbol_has_call(&data, "caller", "foo", "foo");
        assert_symbol_has_call(&data, "caller", "obj.method", "method");
        assert_symbol_has_call(&data, "caller", "std.debug.print", "print");
    }

    #[test]
    fn extracts_rust_receiver_calls_inside_macro_tokens() {
        let source = r#"fn compare(before: Index, after: Index) {
    assert!(before.shares_index_with(&after));
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let calls = extract_calls_full(source, tree.root_node(), 0, source.len(), LangId::Rust);
        let receiver_call = calls
            .iter()
            .find(|(full, _, _, _, _)| full == "before.shares_index_with")
            .expect("receiver call inside macro tokens");
        assert_eq!(receiver_call.1, "shares_index_with");
        assert_eq!(receiver_call.2, 2);
        assert_eq!(
            &source[receiver_call.3..receiver_call.4],
            "before.shares_index_with(&after)"
        );
    }

    #[test]
    fn rust_macro_definitions_do_not_fabricate_receiver_calls() {
        let source = r#"macro_rules! compare {
    ($before:expr, $after:expr) => {
        $before.shares_index_with(&$after)
    };
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let calls = extract_calls_full(source, tree.root_node(), 0, source.len(), LangId::Rust);
        assert!(
            calls
                .iter()
                .all(|(full, _, _, _, _)| full != "$before.shares_index_with"),
            "macro templates are definitions, not call sites: {calls:#?}"
        );
    }

    #[test]
    fn extracts_rust_function_paths_in_value_positions_but_not_direct_calls() {
        let source = r#"
fn callback() {}

fn run() {
    let assigned = callback;
    let tupled = (callback,);
    let arrayed = [callback];
    let holder = Holder { callback };
    consume(crate::callback, assigned, tupled, arrayed, holder);
    callback();
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let refs = extract_rust_value_references(source, tree.root_node());
        let names = refs
            .iter()
            .map(|(full, _, _, _, _)| full.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"crate::callback"), "value refs: {refs:#?}");
        assert_eq!(
            names.iter().filter(|name| **name == "callback").count(),
            4,
            "assignment, tuple, array, and shorthand field should be recorded; refs: {refs:#?}"
        );
    }

    #[test]
    fn rust_value_reference_does_not_bind_shadowing_parameter_or_local() {
        let source = r#"
fn callback() {}
fn consume(_: fn()) {}

fn parameter(callback: fn()) {
    consume(callback);
}

fn local() {
    let callback = || {};
    consume(callback);
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let refs = extract_rust_value_references(source, tree.root_node());

        assert!(
            refs.iter().all(|(_, short, _, _, _)| short != "callback"),
            "shadowing bindings must not resolve to the function item: {refs:#?}"
        );
    }

    #[test]
    fn rust_value_reference_respects_pattern_bindings_and_lexical_scope() {
        let source = r#"
fn callback() {}
fn consume(_: fn()) {}

fn run(maybe: Option<fn()>, callbacks: Vec<fn()>) {
    if let Some(callback) = maybe {
        consume(callback);
    }
    match maybe {
        Some(callback) => consume(callback),
        None => {}
    }
    for callback in callbacks {
        consume(callback);
    }
    let mut values = callbacks;
    while let Some(callback) = values.pop() {
        consume(callback);
    }
    {
        let callback = || {};
        consume(callback);
    }
    consume(callback);
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let refs = extract_rust_value_references(source, tree.root_node());
        let callback_refs = refs
            .iter()
            .filter(|(_, short, _, _, _)| short == "callback")
            .collect::<Vec<_>>();

        assert_eq!(
            callback_refs.len(),
            1,
            "only the function item outside the closed shadowing scope is a value ref: {refs:#?}"
        );
        assert_eq!(callback_refs[0].2, 24);
    }

    #[test]
    fn walk_for_calls_deep_tree_does_not_overflow_bounded_stack() {
        // Regression for the inspect-thread stack overflow / SIGABRT: a
        // pathologically deep expression (here ~6000 nested calls, far past
        // MAX_AST_WALK_DEPTH) must not recurse unbounded. Run on a bounded-stack
        // worker to prove the guard, not the main thread's larger stack.
        let source = deeply_nested_calls(6_000);
        let tree = parse_typescript(&source);
        let call_kinds = call_node_kinds(LangId::TypeScript);
        let mut results = Vec::new();

        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                walk_for_calls(
                    tree.root_node(),
                    &source,
                    0,
                    source.len(),
                    &call_kinds,
                    &mut results,
                );
            })
            .expect("spawn bounded-stack worker")
            .join()
            .expect("deep call walk must not overflow the bounded stack");
    }

    #[test]
    fn collect_type_references_deep_tree_does_not_overflow_bounded_stack() {
        // Regression for the inspect-thread stack overflow / SIGABRT in the
        // dead-code type-reference side channel. Deep generated type chains are
        // treated as opaque past MAX_AST_WALK_DEPTH instead of recursing until
        // the worker stack overflows.
        let source = deeply_nested_type(6_000);
        let tree = parse_typescript(&source);
        let mut results = BTreeSet::new();

        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                collect_type_references(
                    tree.root_node(),
                    &source,
                    0,
                    source.len(),
                    LangId::TypeScript,
                    &mut results,
                );
            })
            .expect("spawn bounded-stack worker")
            .join()
            .expect("deep type-reference walk must not overflow the bounded stack");
    }
}
