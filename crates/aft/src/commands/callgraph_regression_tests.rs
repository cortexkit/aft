use super::*;
use crate::callgraph_store::CallGraphStore;

fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, CallGraphStore) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let mut paths = Vec::new();
    for (file, source) in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, source).unwrap();
        paths.push(path);
    }
    let store = CallGraphStore::open(temp.path().join("store"), root).unwrap();
    store.cold_build(&paths).unwrap();
    (temp, store)
}

fn callers(store: &CallGraphStore, file: &str, symbol: &str, tests: bool) -> String {
    let result = callers_result(store, Path::new(file), symbol, 5, tests).unwrap();
    crate::subc_format::format_callgraph("callers", &serde_json::to_value(result).unwrap(), false)
}

fn impact(store: &CallGraphStore, file: &str, symbol: &str, tests: bool) -> String {
    let result = impact_result(store, Path::new(file), symbol, 5, tests).unwrap();
    crate::subc_format::format_callgraph("impact", &serde_json::to_value(result).unwrap(), false)
}

#[test]
fn callgraph_empty_receiver_note_exact_counts_and_test_filtering() {
    let (_temp, store) = fixture(&[("lib.rs", "fn target() {}\n")]);
    let target = store.node_for(Path::new("lib.rs"), "target").unwrap();
    for include_tests in [false, true] {
        assert!(empty_receiver_note(&store, &target, 0, include_tests)
            .unwrap()
            .is_none());
    }

    let (_temp, store) = fixture(&[
        ("lib.rs", "fn target() {}\nfn caller() { unknown(); }\n#[test]\nfn inline_test() { unknown(); }\n"),
        ("tests/integration.rs", "fn test_caller() { unknown(); }\n"),
    ]);
    let target = store.node_for(Path::new("lib.rs"), "target").unwrap();
    for (include_tests, count, expected) in [
        (false, 1, "Incomplete: 1 call site in the analyzed Rust code could not be resolved, so a dynamic or indirect call may still reach `target`."),
        (true, 3, "Incomplete: 3 call sites in the analyzed Rust code could not be resolved, so a dynamic or indirect call may still reach `target`."),
    ] {
        let note = empty_receiver_note(&store, &target, 0, include_tests).unwrap().unwrap();
        assert_eq!(note.unresolved_call_sites, count);
        assert_eq!(note.incomplete_reason, expected);
        assert!(!note.complete);
        assert!(empty_receiver_note(&store, &target, 1, include_tests).unwrap().is_none());
    }
}

#[test]
fn callgraph_empty_receiver_note_reports_lower_bound_at_cap() {
    let calls = "unknown();\n".repeat(UNRESOLVED_CALL_SITE_COUNT_LIMIT + 17);
    let source = format!("fn target() {{}}\nfn caller() {{\n{calls}}}\n");
    let (_temp, store) = fixture(&[("lib.rs", &source)]);
    for include_tests in [false, true] {
        let result =
            callers_result(&store, Path::new("lib.rs"), "target", 1, include_tests).unwrap();
        let note = result.incomplete.unwrap();
        assert_eq!(note.unresolved_call_sites, 1_000);
        assert_eq!(note.incomplete_reason, "Incomplete: at least 1000 call sites in the analyzed Rust code could not be resolved, so a dynamic or indirect call may still reach `target`.");
        let result =
            impact_result(&store, Path::new("lib.rs"), "target", 1, include_tests).unwrap();
        assert_eq!(
            result.incomplete.unwrap().incomplete_reason,
            note.incomplete_reason
        );
    }
}

#[test]
fn callgraph_callback_references_are_labelled_in_ts_js_and_rust() {
    for (file, source, entry) in [
        (
            "main.ts",
            "function transform() {}\nfunction process() { items.map(transform); }\n",
            "",
        ),
        (
            "main.js",
            "function transform() {}\nfunction process() { items.map(transform); }\n",
            "",
        ),
        (
            "lib.rs",
            "fn transform() {}\nfn process() { spawn(transform); }\n",
            "",
        ),
    ] {
        let (_temp, store) = fixture(&[(file, source)]);
        assert_eq!(
            callers(&store, file, "transform", false),
            format!(
                "1 caller (1 by callback reference) · 1 file group\n{file}\n  ↳ process:2 (callback reference)"
            )
        );
        assert_eq!(impact(&store, file, "transform", false), format!("1 affected site (1 by callback reference) · 1 file\n{file}:2\n  ↳ process{entry} (callback reference)\n  {}", source.lines().nth(1).unwrap()));
        let tree = call_tree_result(&store, Path::new(file), "process", 3, false).unwrap();
        let text = crate::subc_format::format_callgraph(
            "call_tree",
            &serde_json::to_value(tree).unwrap(),
            true,
        );
        assert!(
            !text.contains("↳ transform"),
            "references must not become invocations: {text}"
        );
        let reverse_trace =
            trace_to_result(&store, Path::new(file), "transform", 5, false).unwrap();
        assert!(reverse_trace.paths.is_empty());
    }
}

#[test]
fn callgraph_callback_import_alias_arrow_and_direct_call_stay_distinct() {
    let (_temp, store) = fixture(&[("helper.ts", "export function transform() {}\n"), ("main.ts", "import { transform as callback } from './helper';\nfunction process() { items.map(callback); callback(); }\n")]);
    assert_eq!(callers(&store, "helper.ts", "transform", false), "2 callers (1 by callback reference) · 1 file group\nmain.ts\n  ↳ process:2\n  ↳ process:2 (callback reference)");
    let (_temp, store) = fixture(&[(
        "main.ts",
        "const transform = () => 1;\nfunction process() { items.map(transform); }\n",
    )]);
    assert_eq!(
        callers(&store, "main.ts", "transform", false),
        "1 caller (1 by callback reference) · 1 file group\nmain.ts\n  ↳ process:2 (callback reference)"
    );
    let (_temp, store) = fixture(&[(
        "main.ts",
        "function transform() {}\nfunction process(transform) { items.map(transform); }\n",
    )]);
    let result = callers_result(&store, Path::new("main.ts"), "transform", 5, false).unwrap();
    assert!(
        result.callers.is_empty(),
        "a parameter must not bind to the same-named function"
    );
}

#[test]
fn callgraph_construction_targets_only_explicit_constructor_and_method_owner() {
    for file in ["main.ts", "main.js"] {
        let source = "function helper() {}\nfunction ctorHelper() {}\nclass Service {\n  constructor() { ctorHelper(); }\n  step() { helper(); }\n}\nclass Empty { step() { helper(); } }\nfunction make() { new Service(); new Empty(); }\n";
        let (_temp, store) = fixture(&[(file, source)]);
        assert_eq!(
            callers(&store, file, "Service::constructor", false),
            format!("1 caller · 1 file group\n{file}\n  ↳ make:8")
        );
        assert_eq!(impact(&store, file, "ctorHelper", false), format!("2 affected call sites · 1 file\n{file}:4\n  ↳ Service::constructor\n  constructor() {{ ctorHelper(); }}\n{file}:8\n  ↳ make\n  function make() {{ new Service(); new Empty(); }}"));
        assert_eq!(
            callers(&store, file, "Empty", false),
            "0 callers · 0 file groups"
        );
        assert_eq!(
            callers(&store, file, "helper", false),
            format!("2 callers · 1 file group\n{file}\n  ↳ Empty::step:7\n  ↳ Service::step:5")
        );
        assert_eq!(impact(&store, file, "helper", false), format!("2 affected call sites · 1 file\n{file}:5\n  ↳ Service::step\n  step() {{ helper(); }}\n{file}:7\n  ↳ Empty::step\n  class Empty {{ step() {{ helper(); }} }}"));
    }
    let (_temp, store) = fixture(&[("lib.rs", "fn helper() {}\nstruct Service;\nimpl Service {\n fn new() -> Self { Service }\n fn step() { helper(); }\n}\nfn make() { Service::new(); }\n")]);
    assert_eq!(
        callers(&store, "lib.rs", "Service::new", false),
        "1 caller · 1 file group\nlib.rs\n  ↳ make:7"
    );
    assert_eq!(
        impact(&store, "lib.rs", "helper", false),
        "1 affected call site · 1 file\nlib.rs:5\n  ↳ Service::step\n  fn step() { helper(); }"
    );
}

#[test]
fn callgraph_zero_callers_discloses_unresolved_sites_but_clean_zero_stays_clean() {
    let (_temp, store) = fixture(&[(
        "main.ts",
        "function onlyB() {}\nconst m = { b: onlyB };\nfunction dynamic() { m['b'](); }\n",
    )]);
    assert_eq!(callers(&store, "main.ts", "onlyB", false), "Incomplete: 1 call site in the analyzed TypeScript code could not be resolved, so a dynamic or indirect call may still reach `onlyB`.\n0 callers · 0 file groups");
    let (_temp, store) = fixture(&[("main.ts", "function onlyB() {}\nconst m = { b: onlyB, c: onlyB };\nfunction dynamic() { m['b'](); m['c'](); }\n")]);
    assert_eq!(callers(&store, "main.ts", "onlyB", false), "Incomplete: 2 call sites in the analyzed TypeScript code could not be resolved, so a dynamic or indirect call may still reach `onlyB`.\n0 callers · 0 file groups");
    let (_temp, store) = fixture(&[("main.ts", "function unused() {}\n")]);
    assert_eq!(
        callers(&store, "main.ts", "unused", false),
        "0 callers · 0 file groups"
    );
}

#[test]
fn callgraph_rust_inline_tests_are_hidden_per_item_including_nested_modules() {
    let source = "fn production() {}\nfn live() { production(); }\n#[test]\nfn standalone() { production(); }\n#[cfg(test)]\nmod tests {\n fn support() { super::production(); }\n mod nested { fn check() { crate::production(); } }\n}\n";
    let (_temp, store) = fixture(&[
        ("src/lib.rs", source),
        (
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
    ]);
    assert_eq!(callers(&store, "src/lib.rs", "production", false), "1 caller · 1 file group\nsrc/lib.rs\n  ↳ live:2\n3 callers in tests hidden — includeTests: true shows them");
    assert_eq!(callers(&store, "src/lib.rs", "production", true), "4 callers · 1 file group\nsrc/lib.rs\n  ↳ live:2\n  ↳ standalone:4\n  ↳ tests::nested::check:8\n  ↳ tests::support:7");
    assert_eq!(impact(&store, "src/lib.rs", "production", false), "1 affected call site · 1 file\nsrc/lib.rs:2\n  ↳ live\n  fn live() { production(); }\n3 callers in tests hidden — includeTests: true shows them");
    assert_eq!(impact(&store, "src/lib.rs", "production", true), "4 affected call sites · 1 file\nsrc/lib.rs:2\n  ↳ live\n  fn live() { production(); }\nsrc/lib.rs:4\n  ↳ standalone\n  fn standalone() { production(); }\nsrc/lib.rs:7\n  ↳ tests::support\n  fn support() { super::production(); }\nsrc/lib.rs:8\n  ↳ tests::nested::check\n  mod nested { fn check() { crate::production(); } }");
}

#[test]
fn callgraph_unknown_rust_test_status_is_disclosed_not_guessed() {
    let (_temp, store) = fixture(&[(
        "lib.rs",
        "fn target() {}\nfn caller() { target(); }\nfn broken(\n",
    )]);
    assert_eq!(callers(&store, "lib.rs", "target", false), "Test filtering incomplete: AFT could not tell whether 1 caller site is test code, so it is still shown.\n1 caller · 1 file group\nlib.rs\n  ↳ caller:2");
    assert_eq!(impact(&store, "lib.rs", "target", false), "Test filtering incomplete: AFT could not tell whether 1 caller site is test code, so it is still shown.\n1 affected call site · 1 file\nlib.rs:2\n  ↳ caller\n  fn caller() { target(); }");
}

#[test]
fn callgraph_approved_callback_count_lines_preserve_plain_counts() {
    for (total, references, caller_heading, impact_heading) in [
        (
            1,
            0,
            "1 caller · 1 file group",
            "1 affected call site · 1 file",
        ),
        (
            2,
            0,
            "2 callers · 1 file group",
            "2 affected call sites · 1 file",
        ),
        (
            1,
            1,
            "1 caller (1 by callback reference) · 1 file group",
            "1 affected site (1 by callback reference) · 1 file",
        ),
        (
            2,
            1,
            "2 callers (1 by callback reference) · 1 file group",
            "2 affected sites (1 by callback reference) · 1 file",
        ),
        (
            2,
            2,
            "2 callers (2 by callback reference) · 1 file group",
            "2 affected sites (2 by callback reference) · 1 file",
        ),
    ] {
        let rows = (0..total)
            .map(|index| {
                serde_json::json!({
                    "symbol": format!("process{index}"), "caller_symbol": format!("process{index}"),
                    "caller_file": "main.ts", "line": index + 1,
                    "relation": (index < references).then_some("callback_reference"),
                })
            })
            .collect::<Vec<_>>();
        let callers = serde_json::json!({"total_callers":total, "callback_dependencies":references,
            "callers":[{"file":"main.ts", "callers":rows.clone()}]});
        let impact = serde_json::json!({"total_affected":total, "callback_dependencies":references,
            "affected_files":1, "callers":rows});
        for (operation, result, expected) in [
            ("callers", callers, caller_heading),
            ("impact", impact, impact_heading),
        ] {
            let text = crate::subc_format::format_callgraph(operation, &result, false);
            assert_eq!(text.lines().next().unwrap(), expected);
        }
    }
}

#[test]
fn callgraph_approved_test_status_note_uses_singular_and_plural_wording() {
    assert_eq!(test_filter_note_for_count(1, false).as_deref(), Some("Test filtering incomplete: AFT could not tell whether 1 caller site is test code, so it is still shown."));
    assert_eq!(test_filter_note_for_count(2, false).as_deref(), Some("Test filtering incomplete: AFT could not tell whether 2 caller sites are test code, so they are still shown."));
    assert_eq!(test_filter_note_for_count(0, false), None);
    assert_eq!(test_filter_note_for_count(2, true), None);
}

#[test]
fn callgraph_approved_test_status_note_includes_unknown_macro_callers() {
    for (direct, expected) in [
        ("", "Test filtering incomplete: AFT could not tell whether 2 caller sites are test code, so they are still shown."),
        ("fn direct() { target(); }\n", "Test filtering incomplete: AFT could not tell whether 3 caller sites are test code, so they are still shown."),
    ] {
        let source = format!("fn target() {{}}\nmacro_rules! invoke_target {{ () => {{ target() }} }}\nfn caller() {{ invoke_target!(); }}\n{direct}fn broken(\n");
        let (_temp, store) = fixture(&[("lib.rs", &source)]);
        let result = callers_result(&store, Path::new("lib.rs"), "target", 5, false).unwrap();
        assert_eq!(result.test_filter_note.as_deref(), Some(expected));
    }
}

#[test]
fn callgraph_class_initializer_keeps_its_owner_inside_an_enclosing_function() {
    let source = "function helper() {}\nfunction outer() {\n class Inner {\n  static initial = helper();\n  run() { helper(); }\n }\n}\n";
    let data = crate::callgraph::build_file_data_from_source(Path::new("main.ts"), source).unwrap();
    let (class, _) = data
        .symbol_metadata
        .iter()
        .find(|(_, meta)| meta.kind == crate::symbols::SymbolKind::Class)
        .unwrap();
    assert_eq!(
        data.calls_by_symbol[class]
            .iter()
            .map(|site| site.line)
            .collect::<Vec<_>>(),
        vec![4]
    );
    let (method, _) = data
        .symbol_metadata
        .iter()
        .find(|(_, meta)| meta.kind == crate::symbols::SymbolKind::Method)
        .unwrap();
    assert_eq!(
        data.calls_by_symbol[method]
            .iter()
            .map(|site| site.line)
            .collect::<Vec<_>>(),
        vec![5]
    );
}
