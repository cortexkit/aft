mod tests {
    use crate::data_layer::callgraph_roots::{
        normalize, rust_program_roots, summary, tag, RootKind,
    };
    use crate::data_layer::callgraph_store_adapter::{
        impact_result, trace_to_result, StoreTraceToResult,
    };
    use crate::data_layer::query::{MemorySlices, SliceReader};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The data layer's call graph of a checkout holding `files`.
    fn graph(files: &[(&str, &str)]) -> (tempfile::TempDir, SliceReader<MemorySlices>) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for (path, text) in files {
            write(&root, path, text);
        }
        let reader = crate::data_layer::trace_measure::memory_reader(&root);
        (dir, reader)
    }

    /// Each path as `kind: root > hop > target`, with `*` after each
    /// exported hop past the root.
    fn paths(result: &StoreTraceToResult) -> Vec<String> {
        result
            .paths
            .iter()
            .map(|path| {
                let hops = path
                    .hops
                    .iter()
                    .enumerate()
                    .map(|(index, hop)| {
                        if index > 0 && hop.exported {
                            format!("{}*", hop.symbol)
                        } else {
                            hop.symbol.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" > ");
                format!("{}: {hops}", path.root.wire())
            })
            .collect()
    }

    fn trace(
        reader: &SliceReader<MemorySlices>,
        file: &str,
        symbol: &str,
        include_tests: bool,
    ) -> StoreTraceToResult {
        trace_to_result(reader, Path::new(file), symbol, 10, include_tests)
            .unwrap_or_else(|error| panic!("trace_to {file} {symbol}: {error}"))
    }

    const RUST_LIB: &str = r#"pub mod notbin;

pub fn helper() {}

pub fn run_all() {
    helper();
}

pub struct Worker;

impl Worker {
    pub fn run() {
        helper();
    }
}

#[no_mangle]
pub extern "C" fn ffi_entry() {
    helper();
}

pub fn api() {
    helper();
}

fn unused_private() {
    helper();
}

#[cfg(test)]
mod tests {
    #[test]
    fn calls_helper() {
        super::helper();
    }
}
"#;

    fn rust_crate() -> (tempfile::TempDir, SliceReader<MemorySlices>) {
        graph(&[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", RUST_LIB),
            // A `fn main` in a library module is not where a program starts.
            ("src/notbin.rs", "fn main() {\n    crate::helper();\n}\n"),
            (
                "src/main.rs",
                "use demo::Worker;\n\nfn main() {\n    demo::run_all();\n    Worker::run();\n}\n",
            ),
        ])
    }

    /// A trace follows callers to the binary's `fn main` through exported
    /// functions, which stay in the path; a method named `run` that `main`
    /// calls is not a root; each other root gets its kind; test roots stay
    /// hidden unless tests are included.
    #[test]
    fn rust_trace_ends_at_real_roots_with_their_kinds() {
        let (_dir, reader) = rust_crate();
        let result = trace(&reader, "src/lib.rs", "helper", false);
        assert_eq!(
            paths(&result),
            [
                "main: main > Worker::run* > helper*",
                "main: main > run_all* > helper*",
                "attribute: ffi_entry > helper*",
                "public_api: api > helper*",
                "no_caller_found: main > helper*",
                "no_caller_found: unused_private > helper*",
            ],
            "{result:#?}"
        );
        assert_eq!(result.paths[0].hops[0].file, "src/main.rs");
        assert_eq!(result.paths[4].hops[0].file, "src/notbin.rs");
        assert_eq!(result.paths[2].attribute.as_deref(), Some("no_mangle"));
        assert_eq!(
            result.root_counts,
            BTreeMap::from([
                (RootKind::Main, 1),
                (RootKind::Attribute, 1),
                (RootKind::PublicApi, 1),
                (RootKind::NoCallerFound, 2),
            ])
        );
        assert_eq!(result.total_paths, 7, "the hidden test path counts");
        assert_eq!(result.truncated_paths, 0);
        assert!(!result.max_depth_reached);

        let with_tests = trace(&reader, "src/lib.rs", "helper", true);
        let tests = with_tests
            .paths
            .iter()
            .filter(|path| path.root == RootKind::Test)
            .map(|path| path.hops[0].symbol.as_str())
            .collect::<Vec<_>>();
        assert_eq!(tests.len(), 1, "{with_tests:#?}");
        assert!(tests[0].ends_with("calls_helper"), "{tests:?}");
        assert_eq!(with_tests.root_counts.get(&RootKind::Test), Some(&1));
    }

    /// Impact labels the roots among the affected callers and counts them.
    #[test]
    fn impact_labels_roots_among_affected_callers_and_summarises_them() {
        let (_dir, reader) = rust_crate();
        let result = impact_result(&reader, Path::new("src/lib.rs"), "helper", 5, false).unwrap();
        let labels = result
            .callers
            .iter()
            .map(|caller| {
                format!(
                    "{}:{} {}",
                    caller.caller_file,
                    caller.caller_symbol,
                    caller.root.map_or("-", RootKind::wire)
                )
            })
            .collect::<BTreeSet<_>>();
        let expected = [
            "src/lib.rs:Worker::run -",
            "src/lib.rs:api public_api",
            "src/lib.rs:ffi_entry attribute",
            "src/lib.rs:run_all -",
            "src/lib.rs:unused_private no_caller_found",
            "src/main.rs:main main",
            "src/notbin.rs:main no_caller_found",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
        assert_eq!(labels, expected, "{result:#?}");
        assert_eq!(
            result.root_counts,
            BTreeMap::from([
                (RootKind::Main, 1),
                (RootKind::Attribute, 1),
                (RootKind::PublicApi, 1),
                (RootKind::NoCallerFound, 2),
            ])
        );
        let text = crate::subc_format::format_callgraph(
            "impact",
            &serde_json::to_value(&result).unwrap(),
            false,
        );
        assert!(
            text.contains(
                "Reaches main, 1 function with an entry attribute, 1 public API function, 2 functions with no caller found"
            ),
            "{text}"
        );
        assert!(
            text.contains("  ↳ ffi_entry [root: attribute no_mangle]"),
            "{text}"
        );
    }

    /// The same graph gives the same paths in the same order.
    #[test]
    fn trace_paths_come_in_a_deterministic_order() {
        let (_first_dir, first) = rust_crate();
        let (_second_dir, second) = rust_crate();
        for include_tests in [false, true] {
            let first =
                serde_json::to_string(&trace(&first, "src/lib.rs", "helper", include_tests))
                    .unwrap();
            let second =
                serde_json::to_string(&trace(&second, "src/lib.rs", "helper", include_tests))
                    .unwrap();
            assert_eq!(first, second);
        }
    }

    /// Go starts at `func main` in `package main` only.
    #[test]
    fn go_main_starts_only_in_package_main() {
        let (_dir, reader) = graph(&[
            ("go.mod", "module example.com/demo\n\ngo 1.21\n"),
            (
                "main.go",
                "package main\n\nfunc helper() {}\n\nfunc main() {\n\thelper()\n}\n",
            ),
            (
                "util/util.go",
                "package util\n\nfunc work() {}\n\nfunc main() {\n\twork()\n}\n",
            ),
        ]);
        assert_eq!(
            paths(&trace(&reader, "main.go", "helper", false)),
            ["main: main > helper"]
        );
        assert_eq!(
            paths(&trace(&reader, "util/util.go", "work", false)),
            ["no_caller_found: main > work"]
        );
    }

    /// Go starts every top-level `func init()` at program start, in package
    /// main and in a library package alike; a method named `init` is not
    /// one.
    #[test]
    fn go_init_starts_the_program_in_any_package() {
        let (_dir, reader) = graph(&[
            ("go.mod", "module example.com/demo\n\ngo 1.21\n"),
            (
                "main.go",
                "package main\n\nfunc setup() {}\n\nfunc init() {\n\tsetup()\n}\n\nfunc main() {}\n",
            ),
            (
                "util/util.go",
                "package util\n\nfunc prepare() {}\n\nfunc init() {\n\tprepare()\n}\n\nfunc tidy() {}\n\ntype worker struct{}\n\nfunc (w worker) init() {\n\ttidy()\n}\n",
            ),
        ]);
        assert_eq!(
            paths(&trace(&reader, "main.go", "setup", false)),
            ["main: init > setup"]
        );
        assert_eq!(
            paths(&trace(&reader, "util/util.go", "prepare", false)),
            ["main: init > prepare"]
        );
        let method = trace(&reader, "util/util.go", "tidy", false);
        assert_eq!(method.paths.len(), 1, "{method:#?}");
        assert_eq!(method.paths[0].root, RootKind::NoCallerFound, "{method:#?}");
        assert!(
            method.paths[0].hops[0].symbol.ends_with("init"),
            "{method:#?}"
        );
    }

    /// C starts at `main`.
    #[test]
    fn c_main_starts_the_program() {
        let (_dir, reader) = graph(&[(
            "main.c",
            "static void helper(void) {}\n\nint main(void) {\n    helper();\n    return 0;\n}\n",
        )]);
        assert_eq!(
            paths(&trace(&reader, "main.c", "helper", false)),
            ["main: main > helper"]
        );
    }

    /// A Python module with an `if __name__ == "__main__":` block starts the
    /// program in its top-level code; one without is not a program start.
    #[test]
    fn python_main_block_starts_the_program() {
        let (_dir, reader) = graph(&[
            (
                "app.py",
                "def helper():\n    pass\n\n\ndef main():\n    helper()\n\n\nif __name__ == \"__main__\":\n    main()\n",
            ),
            ("lib.py", "def tool():\n    pass\n\n\ntool()\n"),
        ]);
        assert_eq!(
            paths(&trace(&reader, "app.py", "helper", false)),
            ["main: <top-level> > main > helper"]
        );
        assert_eq!(
            paths(&trace(&reader, "lib.py", "tool", false)),
            ["no_caller_found: <top-level> > tool"]
        );
    }

    /// TypeScript has no starting function: an exported function nothing
    /// calls is public API, and an unexported one has no caller found.
    #[test]
    fn typescript_roots_are_public_api_or_no_caller_found() {
        let (_dir, reader) = graph(&[(
            "app.ts",
            "function helper() {}\n\nexport function api() {\n  helper();\n}\n\nfunction hidden() {\n  helper();\n}\n\nfunction main() {\n  helper();\n}\n",
        )]);
        assert_eq!(
            paths(&trace(&reader, "app.ts", "helper", false)),
            [
                "public_api: api > helper",
                "no_caller_found: hidden > helper",
                "no_caller_found: main > helper",
            ]
        );
    }

    #[test]
    fn rust_program_roots_follow_cargo_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"demo\"\n\n[[bin]]\nname = \"tool\"\npath = \"tools/tool.rs\"\n\n[[test]]\nname = \"plain\"\n\n[[test]]\nname = \"custom\"\nharness = false\n",
        );
        for file in [
            "src/main.rs",
            "src/lib.rs",
            "src/bin/extra.rs",
            "src/bin/multi/main.rs",
            "tools/tool.rs",
            "examples/demo.rs",
            "build.rs",
            "tests/plain.rs",
            "tests/custom.rs",
            "src/helpers.rs",
        ] {
            write(root, file, "fn main() {}\n");
        }
        let roots = rust_program_roots(root);
        let expected = [
            "build.rs",
            "examples/demo.rs",
            "src/bin/extra.rs",
            "src/bin/multi/main.rs",
            "src/main.rs",
            "tests/custom.rs",
            "tools/tool.rs",
        ]
        .into_iter()
        .map(|file| normalize(&root.join(file)))
        .collect::<BTreeSet<_>>();
        assert_eq!(roots, expected);

        // A virtual workspace manifest owns no targets.
        write(root, "ws/Cargo.toml", "[workspace]\nmembers = []\n");
        write(root, "ws/src/main.rs", "fn main() {}\n");
        assert!(rust_program_roots(&root.join("ws")).is_empty());
    }

    #[test]
    fn summaries_name_kinds_in_order_with_counts() {
        let counts = serde_json::json!({"public_api": 2, "test": 4, "main": 1});
        assert_eq!(
            summary(counts.as_object().unwrap()).as_deref(),
            Some("main, 4 tests, 2 public API functions")
        );
        let counts = serde_json::json!({"no_caller_found": 1, "attribute": 3, "main": 2});
        assert_eq!(
            summary(counts.as_object().unwrap()).as_deref(),
            Some("2 main functions, 3 functions with an entry attribute, 1 function with no caller found")
        );
        assert_eq!(summary(&serde_json::Map::new()), None);
        assert_eq!(
            tag("attribute", Some("no_mangle")).as_deref(),
            Some("[root: attribute no_mangle]")
        );
        assert_eq!(
            tag("public_api", None).as_deref(),
            Some("[root: public API]")
        );
    }
}
