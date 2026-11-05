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

fn assert_python_view_answers(
    store: &CallGraphStore,
    targets: &[(&str, &str)],
    roots: &[(&str, &str)],
) {
    fn python_files(root: &Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("view-fixture-")
                {
                    python_files(&path, files);
                }
            } else if path.extension().is_some_and(|extension| extension == "py") {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    python_files(store.project_root(), &mut files);
    files.sort();
    let view = crate::callgraph_store::fixture_manifest_view_for_test(store.project_root(), &files);
    for (file, symbol) in targets {
        let legacy = callers_result(store, Path::new(file), symbol, 1, true).unwrap();
        let viewed = callers_result(&view, Path::new(file), symbol, 1, true).unwrap();
        assert_eq!(
            serde_json::to_value(legacy).unwrap(),
            serde_json::to_value(viewed).unwrap(),
            "callers {file}:{symbol}"
        );
    }
    for (file, symbol) in roots {
        let legacy = call_tree_result(store, Path::new(file), symbol, 2, true).unwrap();
        let viewed = call_tree_result(&view, Path::new(file), symbol, 2, true).unwrap();
        assert_eq!(
            serde_json::to_value(legacy).unwrap(),
            serde_json::to_value(viewed).unwrap(),
            "call_tree {file}:{symbol}"
        );
    }
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
fn callgraph_python_import_reporter_fixture() {
    let (_temp, store) = fixture(&[
        ("mod.py", "def toplevel_target():\n    pass\n\ndef local_target():\n    pass\n\ndef test_target():\n    pass\n\ndef same_file_caller():\n    toplevel_target()\n"),
        ("caller_toplevel.py", "from mod import toplevel_target\n\ndef run_toplevel():\n    toplevel_target()\n"),
        ("caller_local.py", "def run_local():\n    from mod import local_target\n    local_target()\n"),
        ("tests/test_probe.py", "class TestProbe:\n    def test_local(self):\n        from mod import test_target\n        test_target()\n"),
        ("ts_mod.ts", "export function tsTarget(): number {\n  return 7;\n}\n"),
        ("ts_caller.ts", "import { tsTarget } from \"./ts_mod\";\n\nexport function runTsCaller(): number {\n  return tsTarget();\n}\n"),
    ]);
    let results = [
        callers(&store, "mod.py", "toplevel_target", false),
        callers(&store, "mod.py", "local_target", false),
        callers(&store, "mod.py", "test_target", true),
    ];
    assert_eq!(
        callers(&store, "ts_mod.ts", "tsTarget", false),
        "1 caller · 1 file group\nts_caller.ts\n  ↳ runTsCaller:4"
    );
    for (target, result) in ["toplevel_target", "local_target", "test_target"]
        .iter()
        .zip(&results)
    {
        println!("callers {target}:\n{result}");
    }
    for (file, symbol) in [
        ("caller_toplevel.py", "run_toplevel"),
        ("caller_local.py", "run_local"),
    ] {
        let tree = call_tree_result(&store, Path::new(file), symbol, 3, false).unwrap();
        let text = crate::subc_format::format_callgraph(
            "call_tree",
            &serde_json::to_value(tree).unwrap(),
            false,
        );
        println!("call_tree {symbol}:\n{text}");
        assert!(text.contains("[mod.py:"), "{text}");
        assert!(!text.contains("unresolved"), "{text}");
    }
    assert!(results[0].starts_with("2 callers"), "{}", results[0]);
    assert!(results[0].contains("same_file_caller:11"), "{}", results[0]);
    assert!(results[0].contains("run_toplevel:4"), "{}", results[0]);
    assert!(results[1].starts_with("1 caller"), "{}", results[1]);
    assert!(results[1].contains("run_local:3"), "{}", results[1]);
    assert!(results[2].starts_with("1 caller"), "{}", results[2]);
    assert!(
        results[2].contains("TestProbe::test_local:4"),
        "{}",
        results[2]
    );
    assert_eq!(
        callers(&store, "mod.py", "test_target", false),
        "0 callers · 0 file groups\n1 callers in tests hidden — includeTests: true shows them"
    );
}

#[test]
fn callgraph_python_import_forms_and_unrelated_names() {
    let (_temp, store) = fixture(&[
        ("mod.py", "def f():\n    pass\n"),
        ("pkg/__init__.py", ""),
        ("pkg/mod.py", "def f():\n    pass\n"),
        ("pkg/sub/__init__.py", ""),
        ("unrelated.py", "def f():\n    pass\n"),
        ("method_only.py", "class Only:\n    def f(self):\n        pass\n"),
        ("not_importable.py", "from method_only import f\n\ndef not_importable():\n    f()\n"),
        ("root_calls.py", "from mod import f as renamed\nimport mod\n\ndef root_named():\n    renamed()\n\ndef root_module():\n    mod.f()\n\ndef local_module():\n    import mod as m\n    m.f()\n"),
        ("pkg/calls.py", "from pkg.mod import f\nfrom .mod import f as relative_f\nimport pkg.mod\n\ndef absolute():\n    f()\n\ndef relative():\n    relative_f()\n\ndef dotted():\n    pkg.mod.f()\n\ndef local():\n    from .mod import f as local_f\n    local_f()\n\ndef sibling():\n    local_f()\n\ndef shadow(f):\n    f()\n"),
        ("pkg/sub/calls.py", "from ..mod import f\n\ndef parent():\n    f()\n"),
        ("missing.py", "from absent import f\n\ndef missing():\n    f()\n"),
        ("invalid_relative.py", "from .mod import f\n\ndef invalid():\n    f()\n"),
    ]);
    let root = callers(&store, "mod.py", "f", true);
    assert!(root.contains("3 callers ·"), "{root}");
    for name in ["root_named", "root_module", "local_module"] {
        assert!(root.contains(name), "{root}");
    }
    let package = callers(&store, "pkg/mod.py", "f", true);
    assert!(package.contains("5 callers ·"), "{package}");
    for name in ["absolute", "relative", "dotted", "local", "parent"] {
        assert!(package.contains(&format!("↳ {name}:")), "{package}");
    }
    for name in ["sibling", "shadow", "missing", "invalid", "root_named"] {
        assert!(!package.contains(&format!("↳ {name}:")), "{package}");
    }
    let unrelated = callers(&store, "unrelated.py", "f", true);
    assert!(unrelated.starts_with("Incomplete:"), "{unrelated}");
    assert!(unrelated.contains("analyzed Python code"), "{unrelated}");
    assert!(
        unrelated.ends_with("0 callers · 0 file groups"),
        "{unrelated}"
    );
    let method = callers(&store, "method_only.py", "Only::f", true);
    assert!(method.starts_with("Incomplete:"), "{method}");
    assert!(method.ends_with("0 callers · 0 file groups"), "{method}");
}

#[test]
fn callgraph_python_reexport_chains_are_lexical_and_refreshable() {
    let (_temp, store) = fixture(&[
        ("pkg/source.py", "def target():\n    pass\n\ndef replacement():\n    pass\n"),
        ("pkg/middle.py", "from .source import target as forwarded\n"),
        ("pkg/__init__.py", "from .middle import forwarded as public\n"),
        ("pkg.py", "def public():\n    pass\n"),
        ("calls.py", "from pkg import public as renamed\nimport pkg as namespace\ndef named():\n    renamed()\ndef qualified():\n    namespace.public()\ndef shadow(renamed):\n    renamed()\n"),
        ("hidden.py", "def setup():\n    from pkg.source import target as public\n"),
        ("shadowed.py", "from pkg.source import target as public\npublic = unknown\n"),
        ("ambiguous.py", "if condition:\n    from pkg.source import target as public\nelse:\n    from pkg.source import replacement as public\n"),
        ("cycle_a.py", "from cycle_b import public\n"),
        ("cycle_b.py", "from cycle_a import public\n"),
        ("deleted.py", "from pkg.source import target as public\ndel public\n"),
        ("with_alias.py", "from pkg.source import target as public\nwith unknown() as public:\n    pass\n"),
        ("header_shadow.py", "from pkg.source import target as public\ndef setup(value=(public := unknown)):\n    pass\n"),
        ("negative.py", "from hidden import public as hidden\nfrom shadowed import public as shadowed\nfrom ambiguous import public as ambiguous\nfrom cycle_a import public as cyclic\nfrom deleted import public as deleted\nfrom with_alias import public as with_alias\nfrom header_shadow import public as header_shadow\ndef rejected():\n    hidden()\n    shadowed()\n    ambiguous()\n    cyclic()\n    deleted()\n    with_alias()\n    header_shadow()\n"),
    ]);
    let sites = store
        .direct_callers_of(Path::new("pkg/source.py"), "target")
        .unwrap();
    let mut names = sites
        .iter()
        .map(|site| site.caller.symbol.as_str())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["named", "qualified"]);
    assert!(sites.iter().all(|site| !site.approximate()));
    let targets = [
        ("pkg/source.py", "target"),
        ("pkg/source.py", "replacement"),
        ("pkg.py", "public"),
    ];
    assert!(store
        .direct_callers_of(Path::new("pkg.py"), "public")
        .unwrap()
        .is_empty());
    let roots = [
        ("calls.py", "named"),
        ("calls.py", "qualified"),
        ("calls.py", "shadow"),
        ("negative.py", "rejected"),
    ];
    assert_python_view_answers(&store, &targets, &roots);
    let middle = store.project_root().join("pkg/middle.py");
    std::fs::write(&middle, "from .source import replacement as forwarded\n").unwrap();
    store.refresh_files(&[middle]).unwrap();
    assert!(store
        .direct_callers_of(Path::new("pkg/source.py"), "target")
        .unwrap()
        .is_empty());
    let sites = store
        .direct_callers_of(Path::new("pkg/source.py"), "replacement")
        .unwrap();
    assert_eq!(sites.len(), 2);
    assert!(sites.iter().all(|site| !site.approximate()));
    assert_python_view_answers(&store, &targets, &roots);
}

#[test]
fn callgraph_python_view_memo_separates_local_imports_and_shadows() {
    let (_temp, store) = fixture(&[
        ("one.py", "def target():\n    pass\n"),
        ("two.py", "def target():\n    pass\n"),
        ("calls.py", "def first():\n    from one import target as pick\n    pick()\ndef second():\n    from two import target as pick\n    pick()\ndef shadow(pick):\n    pick()\n"),
    ]);
    for (file, caller) in [("one.py", "first"), ("two.py", "second")] {
        let sites = store.direct_callers_of(Path::new(file), "target").unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].caller.symbol, caller);
    }
    assert_python_view_answers(
        &store,
        &[("one.py", "target"), ("two.py", "target")],
        &[
            ("calls.py", "first"),
            ("calls.py", "second"),
            ("calls.py", "shadow"),
        ],
    );
}

#[test]
fn callgraph_python_definition_headers_use_defining_scope() {
    let (_temp, store) = fixture(&[
        ("mod.py", "def factory():\n    pass\n"),
        ("calls.py", "from mod import factory\ndef outer():\n    @factory()\n    def inner(factory=factory(), annotation: factory() = None) -> factory():\n        factory()\n    return inner\n\nclass Container:\n    from mod import factory as local_factory\n    def method(self, local_factory=local_factory()):\n        local_factory()\n"),
    ]);
    let sites = store
        .direct_callers_of(Path::new("mod.py"), "factory")
        .unwrap();
    let mut owners = sites
        .iter()
        .map(|site| (site.line, site.caller.symbol.as_str()))
        .collect::<Vec<_>>();
    owners.sort();
    assert_eq!(
        owners,
        [
            (3, "outer"),
            (4, "outer"),
            (4, "outer"),
            (4, "outer"),
            (10, "Container")
        ]
    );
    assert!(sites.iter().all(|site| !site.approximate()));
    assert_python_view_answers(
        &store,
        &[("mod.py", "factory")],
        &[
            ("calls.py", "outer"),
            ("calls.py", "inner"),
            ("calls.py", "Container::method"),
        ],
    );
}

#[test]
fn callgraph_python_nested_bodies_keep_enclosing_callers_and_lexical_headers() {
    let (_temp, store) = fixture(&[
        ("target.py", "def header():\n    pass\ndef body():\n    pass\n"),
        ("calls.py", "from target import header, body\ndef outer():\n    @header()\n    def middle(value=header()):\n        def inner(value=header()):\n            body()\n        return inner\n    return middle\n"),
    ]);
    let owners = |symbol| {
        let mut sites = store
            .direct_callers_of(Path::new("target.py"), symbol)
            .unwrap()
            .into_iter()
            .map(|site| {
                assert!(!site.approximate());
                (site.line, site.caller.symbol)
            })
            .collect::<Vec<_>>();
        sites.sort();
        sites
    };
    assert_eq!(
        owners("body"),
        [
            (6, "inner".into()),
            (6, "middle".into()),
            (6, "outer".into())
        ]
    );
    assert_eq!(
        owners("header"),
        [
            (3, "outer".into()),
            (4, "outer".into()),
            (5, "middle".into()),
            (5, "outer".into())
        ]
    );
    assert_python_view_answers(
        &store,
        &[("target.py", "header"), ("target.py", "body")],
        &[
            ("calls.py", "outer"),
            ("calls.py", "middle"),
            ("calls.py", "inner"),
        ],
    );
}

#[test]
fn callgraph_python_nested_receiver_bodies_keep_enclosing_callers_in_views() {
    let (_temp, store) = fixture(&[(
        "calls.py",
        "class Builder:\n    def make(self): pass\nbuilder = Builder()\ndef outer():\n    def inner(value=builder.make()):\n        builder.make()\n    return inner\n",
    )]);
    let view = crate::callgraph_store::fixture_manifest_view_for_test(
        store.project_root(),
        &[store.project_root().join("calls.py")],
    );
    // Module-level assignments do not prove a function-local receiver type.
    // Preserve all unresolved caller evidence without inventing an exact target.
    assert!(view
        .direct_callers_of(Path::new("calls.py"), "Builder::make")
        .unwrap()
        .is_empty());
    let snapshot = crate::callgraph_store::project_dead_code_snapshot_from_view(&view)
        .unwrap()
        .1;
    let mut owners = snapshot
        .outbound_calls
        .iter()
        .filter(|call| matches!(call.line, 5 | 6))
        .map(|call| (call.line, call.caller_symbol.as_str()))
        .collect::<Vec<_>>();
    owners.sort();
    assert_eq!(owners, [(5, "outer"), (6, "inner"), (6, "outer")]);
    let counts = view.dispatch_site_counts().unwrap().unwrap();
    assert_eq!(counts["python"].unresolved_receiver_sites, 3);
}

#[test]
fn callgraph_python_constructors_follow_class_bindings_not_names() {
    let (_temp, store) = fixture(&[
        ("pkg/source.py", "class Widget:\n    def __init__(self):\n        pass\n\nclass WithoutInit:\n    pass\n"),
        ("pkg/__init__.py", "from .source import Widget as Public\n"),
        ("calls.py", "from pkg import Public as Renamed\nimport pkg.source as module\nclass Local:\n    def __init__(self):\n        pass\ndef construct():\n    Local()\n    Renamed()\n    module.Widget()\ndef shadow(Renamed):\n    Renamed()\n"),
        ("unrelated.py", "class Renamed:\n    def __init__(self):\n        pass\n"),
        ("duplicate.py", "class Duplicate:\n    def __init__(self):\n        pass\ndef Duplicate():\n    pass\ndef rejected():\n    Duplicate()\n"),
        ("local_classes.py", "def left():\n    class Same:\n        def __init__(self):\n            pass\n    Same()\ndef right():\n    class Same:\n        def __init__(self):\n            pass\n    Same()\n"),
    ]);
    let sites = store
        .direct_callers_of(Path::new("pkg/source.py"), "Widget::__init__")
        .unwrap();
    assert_eq!(sites.len(), 2);
    assert!(sites
        .iter()
        .all(|site| site.caller.symbol == "construct" && !site.approximate()));
    let local = store
        .direct_callers_of(Path::new("calls.py"), "Local::__init__")
        .unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].caller.symbol, "construct");
    assert!(store
        .direct_callers_of(Path::new("unrelated.py"), "Renamed::__init__")
        .unwrap()
        .is_empty());
    assert!(store
        .direct_callers_of(Path::new("duplicate.py"), "Duplicate::__init__")
        .unwrap()
        .is_empty());
    assert!(store
        .direct_callers_of(Path::new("local_classes.py"), "Same::__init__")
        .unwrap()
        .is_empty());
    assert_python_view_answers(
        &store,
        &[
            ("pkg/source.py", "Widget::__init__"),
            ("calls.py", "Local::__init__"),
            ("unrelated.py", "Renamed::__init__"),
            ("duplicate.py", "Duplicate::__init__"),
            ("local_classes.py", "Same::__init__"),
        ],
        &[
            ("calls.py", "construct"),
            ("calls.py", "shadow"),
            ("local_classes.py", "left"),
            ("local_classes.py", "right"),
        ],
    );
}

#[test]
fn callgraph_nonzero_callers_disclose_only_matching_unresolved_names() {
    let (_temp, store) = fixture(&[
        (
            "mod.py",
            "def target():\n    pass\n\ndef known():\n    target()\n",
        ),
        (
            "unknown.py",
            "from missing import target\n\ndef unknown():\n    target()\n    unrelated()\n",
        ),
        (
            "tests/test_probe.py",
            "def test_unknown():\n    from missing import target\n    target()\n",
        ),
        ("other.ts", "function other() { target(); }\n"),
    ]);
    for (tests, count, expected) in [
        (false, 1, "Incomplete: 1 call site named `target` in the analyzed Python code could not be resolved; additional callers may be missing."),
        (true, 2, "Incomplete: 2 call sites named `target` in the analyzed Python code could not be resolved; additional callers may be missing."),
    ] {
        let result = callers_result(&store, Path::new("mod.py"), "target", 1, tests).unwrap();
        assert_eq!(result.total_callers, 1);
        let note = result.incomplete.unwrap();
        assert!(!note.complete);
        assert_eq!(note.unresolved_call_sites, count);
        assert_eq!(note.incomplete_reason, expected);
        let result = impact_result(&store, Path::new("mod.py"), "target", 1, tests).unwrap();
        assert_eq!(result.incomplete.unwrap().incomplete_reason, expected);
        assert!(callers(&store, "mod.py", "target", tests).starts_with(expected));
    }
    let (_temp, store) = fixture(&[(
        "mod.py",
        "def target():\n    pass\n\ndef known():\n    target()\n    unrelated()\n",
    )]);
    assert!(
        callers_result(&store, Path::new("mod.py"), "target", 1, false)
            .unwrap()
            .incomplete
            .is_none()
    );
}

#[test]
fn callgraph_nonzero_matching_unresolved_count_is_a_lower_bound_at_cap() {
    let calls = "    target()\n".repeat(UNRESOLVED_CALL_SITE_COUNT_LIMIT + 17);
    let source = format!("from missing import target\n\ndef unknown():\n{calls}");
    let (_temp, store) = fixture(&[
        (
            "mod.py",
            "def target():\n    pass\n\ndef known():\n    target()\n",
        ),
        ("unknown.py", &source),
    ]);
    let result = callers_result(&store, Path::new("mod.py"), "target", 1, false).unwrap();
    assert_eq!(result.total_callers, 1);
    let note = result.incomplete.unwrap();
    assert_eq!(note.unresolved_call_sites, 1_000);
    assert_eq!(note.incomplete_reason, "Incomplete: at least 1000 call sites named `target` in the analyzed Python code could not be resolved; additional callers may be missing.");
}

#[test]
fn callgraph_callback_references_are_labelled_in_ts_js_and_rust() {
    for (file, source, entry) in [
        (
            "main.ts",
            "function transform() {}\nfunction process() { items.map(transform); }\n",
            " [root: no caller found]",
        ),
        (
            "main.js",
            "function transform() {}\nfunction process() { items.map(transform); }\n",
            " [root: no caller found]",
        ),
        (
            "lib.rs",
            "fn transform() {}\nfn process() { spawn(transform); }\n",
            " [root: no caller found]",
        ),
    ] {
        let (_temp, store) = fixture(&[(file, source)]);
        assert_eq!(
            callers(&store, file, "transform", false),
            format!(
                "1 caller (1 by callback reference) · 1 file group\n{file}\n  ↳ process:2 (callback reference)"
            )
        );
        // `process` is a root: nothing calls it.
        assert_eq!(impact(&store, file, "transform", false), format!("1 affected site (1 by callback reference) · 1 file\nReaches 1 function with no caller found\n{file}:2\n  ↳ process{entry} (callback reference)\n  {}", source.lines().nth(1).unwrap()));
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
        // A callback reference is not a call, so the reverse trace does not
        // follow it: `transform` itself is the root, with no caller found.
        let reverse_trace =
            trace_to_result(&store, Path::new(file), "transform", 5, false).unwrap();
        assert_eq!(reverse_trace.paths.len(), 1);
        assert_eq!(
            reverse_trace.paths[0].root,
            crate::commands::callgraph_roots::RootKind::NoCallerFound
        );
        assert_eq!(reverse_trace.paths[0].hops.len(), 1);
        assert_eq!(reverse_trace.paths[0].hops[0].symbol, "transform");
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
        // `make` is a root, the constructor it calls is not.
        assert_eq!(impact(&store, file, "ctorHelper", false), format!("2 affected call sites · 1 file\nReaches 1 function with no caller found\n{file}:4\n  ↳ Service::constructor\n  constructor() {{ ctorHelper(); }}\n{file}:8\n  ↳ make [root: no caller found]\n  function make() {{ new Service(); new Empty(); }}"));
        assert_eq!(
            callers(&store, file, "Empty", false),
            "0 callers · 0 file groups"
        );
        assert_eq!(
            callers(&store, file, "helper", false),
            format!("2 callers · 1 file group\n{file}\n  ↳ Empty::step:7\n  ↳ Service::step:5")
        );
        assert_eq!(impact(&store, file, "helper", false), format!("2 affected call sites · 1 file\nReaches 2 functions with no caller found\n{file}:5\n  ↳ Service::step [root: no caller found]\n  step() {{ helper(); }}\n{file}:7\n  ↳ Empty::step [root: no caller found]\n  class Empty {{ step() {{ helper(); }} }}"));
    }
    let (_temp, store) = fixture(&[("lib.rs", "fn helper() {}\nstruct Service;\nimpl Service {\n fn new() -> Self { Service }\n fn step() { helper(); }\n}\nfn make() { Service::new(); }\n")]);
    assert_eq!(
        callers(&store, "lib.rs", "Service::new", false),
        "1 caller · 1 file group\nlib.rs\n  ↳ make:7"
    );
    assert_eq!(
        impact(&store, "lib.rs", "helper", false),
        "1 affected call site · 1 file\nReaches 1 function with no caller found\nlib.rs:5\n  ↳ Service::step [root: no caller found]\n  fn step() { helper(); }"
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
    // Every caller is a root; the test ones are labelled tests and counted
    // only when tests are included.
    assert_eq!(impact(&store, "src/lib.rs", "production", false), "1 affected call site · 1 file\nReaches 1 function with no caller found\nsrc/lib.rs:2\n  ↳ live [root: no caller found]\n  fn live() { production(); }\n3 callers in tests hidden — includeTests: true shows them");
    assert_eq!(impact(&store, "src/lib.rs", "production", true), "4 affected call sites · 1 file\nReaches 3 tests, 1 function with no caller found\nsrc/lib.rs:2\n  ↳ live [root: no caller found]\n  fn live() { production(); }\nsrc/lib.rs:4\n  ↳ standalone [root: test]\n  fn standalone() { production(); }\nsrc/lib.rs:7\n  ↳ tests::support [root: test]\n  fn support() { super::production(); }\nsrc/lib.rs:8\n  ↳ tests::nested::check [root: test]\n  mod nested { fn check() { crate::production(); } }");
}

#[test]
fn callgraph_unknown_rust_test_status_is_disclosed_not_guessed() {
    let (_temp, store) = fixture(&[(
        "lib.rs",
        "fn target() {}\nfn caller() { target(); }\nfn broken(\n",
    )]);
    assert_eq!(callers(&store, "lib.rs", "target", false), "Test filtering incomplete: AFT could not tell whether 1 caller site is test code, so it is still shown.\n1 caller · 1 file group\nlib.rs\n  ↳ caller:2");
    assert_eq!(impact(&store, "lib.rs", "target", false), "Test filtering incomplete: AFT could not tell whether 1 caller site is test code, so it is still shown.\n1 affected call site · 1 file\nReaches 1 function with no caller found\nlib.rs:2\n  ↳ caller [root: no caller found]\n  fn caller() { target(); }");
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
