//! Where `trace_to` and `impact` say execution starts.
//!
//! A trace follows callers until none are left; the function it stops at is
//! a root. Each root gets one kind, from the facts extraction recorded:
//!
//! - `main`: where the program starts, as its language defines it: `fn main`
//!   at the root of a Rust binary target, `func main` in a Go `package
//!   main` and a top-level `func init()` in any Go package (the runtime
//!   runs it at start-up; code never calls it), `main` in C, C++ and
//!   Objective-C, and the top-level code of a Python module that has an
//!   `if __name__ == "__main__":` block. JavaScript and TypeScript define
//!   no starting function.
//! - `test`: test code, by test attribute or test file.
//! - `attribute`: an entry attribute extraction recorded, such as
//!   `#[no_mangle]` or `#[tauri::command]`.
//! - `public API`: an exported function no caller in the repository calls.
//! - `no caller found`: none of the above, and nothing in the repository
//!   calls it: a callback, a trait object, reflection, or dead code.
//!
//! Function names are never guessed at: a method called `run` is a root only
//! when nothing calls it. Dead-code analysis keeps its own, more cautious
//! rule (`callgraph::is_entry_point`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::callgraph_store::StoreNode;
use crate::inspect::job::is_test_file;
use crate::parser::LangId;

/// The kind of a root: why a path of callers ends at it. Declared in the
/// order replies list kinds and sort paths by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// Where the program starts, as its language defines it.
    Main,
    /// Test code.
    Test,
    /// An entry attribute marks the function as called from outside the
    /// program.
    Attribute,
    /// An exported function no caller in the repository calls.
    PublicApi,
    /// Not exported, and nothing in the repository calls it.
    NoCallerFound,
}

impl RootKind {
    const ALL: [Self; 5] = [
        Self::Main,
        Self::Test,
        Self::Attribute,
        Self::PublicApi,
        Self::NoCallerFound,
    ];

    /// The kind's name in replies (`main`, `public_api`).
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Test => "test",
            Self::Attribute => "attribute",
            Self::PublicApi => "public_api",
            Self::NoCallerFound => "no_caller_found",
        }
    }

    /// The kind its name in a reply names.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.wire() == name)
    }

    /// The kind as rendered text shows it on a root.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Test => "test",
            Self::Attribute => "attribute",
            Self::PublicApi => "public API",
            Self::NoCallerFound => "no caller found",
        }
    }

    /// `count` roots of this kind, as a summary line names them.
    fn counted(self, count: usize) -> String {
        let one = count == 1;
        match self {
            Self::Main if one => "main".to_string(),
            Self::Main => format!("{count} main functions"),
            Self::Test => format!("{count} test{}", if one { "" } else { "s" }),
            Self::Attribute => format!(
                "{count} function{} with an entry attribute",
                if one { "" } else { "s" }
            ),
            Self::PublicApi => format!("{count} public API function{}", if one { "" } else { "s" }),
            Self::NoCallerFound => format!(
                "{count} function{} with no caller found",
                if one { "" } else { "s" }
            ),
        }
    }
}

/// The roots a reply reached, counted once per function and kind.
#[derive(Debug, Default)]
pub(crate) struct RootTally {
    seen: BTreeSet<(RootKind, String, String)>,
}

impl RootTally {
    pub(crate) fn add(&mut self, kind: RootKind, node: &StoreNode) {
        self.seen
            .insert((kind, node.file.clone(), node.symbol.clone()));
    }

    /// How many distinct roots of each kind were added; kinds with none are
    /// left out.
    pub(crate) fn counts(&self) -> BTreeMap<RootKind, usize> {
        let mut counts = BTreeMap::new();
        for (kind, _, _) in &self.seen {
            *counts.entry(*kind).or_default() += 1;
        }
        counts
    }
}

/// The summary of a reply's `root_counts` object, kinds in [`RootKind`]
/// order: `main, 4 tests, 2 public API functions`. `None` when it names no
/// root.
#[must_use]
pub fn summary(counts: &serde_json::Map<String, Value>) -> Option<String> {
    let parts = RootKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let count = counts.get(kind.wire())?.as_u64()?;
            (count > 0).then(|| kind.counted(count as usize))
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The tag rendered text puts on a root: `[root: main]`, or for an entry
/// attribute `[root: attribute no_mangle]`.
#[must_use]
pub fn tag(kind: &str, attribute: Option<&str>) -> Option<String> {
    let kind = RootKind::from_wire(kind)?;
    Some(match (kind, attribute) {
        (RootKind::Attribute, Some(attribute)) => format!("[root: attribute {attribute}]"),
        _ => format!("[root: {}]", kind.label()),
    })
}

/// Labels the roots one reply reaches. It reads a Rust package's Cargo
/// manifest the first time a `fn main` in it is a root, to tell binary
/// targets from libraries, and keeps the answer for the rest of the reply.
pub(crate) struct Roots<'a> {
    project_root: &'a Path,
    /// Per package directory, the root files of its program targets.
    rust_programs: HashMap<PathBuf, BTreeSet<PathBuf>>,
}

impl<'a> Roots<'a> {
    pub(crate) fn new(project_root: &'a Path) -> Self {
        Self {
            project_root,
            rust_programs: HashMap::new(),
        }
    }

    /// The kind of `node` as a root. `caller_is_test` is the test fact the
    /// call site that reached it carried, when there was one.
    pub(crate) fn kind(&mut self, node: &StoreNode, caller_is_test: Option<bool>) -> RootKind {
        if is_test_file(&node.file)
            || node.entry.is_test == Some(true)
            || caller_is_test == Some(true)
        {
            RootKind::Test
        } else if node.entry.program_start
            && (node.lang != LangId::Rust || self.is_rust_program_root(&node.file))
        {
            RootKind::Main
        } else if node.entry.entry_attribute.is_some() {
            RootKind::Attribute
        } else if node.exported || node.entry.default_export {
            RootKind::PublicApi
        } else {
            RootKind::NoCallerFound
        }
    }

    /// Whether `file` (relative to the project root) is the root file of a
    /// target Cargo builds into a program.
    fn is_rust_program_root(&mut self, file: &str) -> bool {
        let path = normalize(&self.project_root.join(file));
        let Some(package) = owning_manifest_dir(&normalize(self.project_root), &path) else {
            return false;
        };
        self.rust_programs
            .entry(package)
            .or_insert_with_key(|package| rust_program_roots(package))
            .contains(&path)
    }
}

/// `path` with `.` components dropped and `..` applied, without reading the
/// file system.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// The directory of the nearest `Cargo.toml` above `file`, no higher than
/// `project_root`: the package `file` belongs to.
fn owning_manifest_dir(project_root: &Path, file: &Path) -> Option<PathBuf> {
    let mut directory = file.parent();
    while let Some(current) = directory {
        if !current.starts_with(project_root) {
            return None;
        }
        if current.join("Cargo.toml").is_file() {
            return Some(current.to_path_buf());
        }
        directory = current.parent();
    }
    None
}

/// The root files of the targets Cargo builds into programs in the package
/// at `directory`: its binaries (`[[bin]]` entries and, unless `autobins` is
/// off, `src/main.rs`, `src/bin/*.rs` and `src/bin/*/main.rs`), its
/// examples, its build script, and test or bench targets that turn the test
/// harness off. Empty when the manifest cannot be read or declares no
/// `[package]`.
fn rust_program_roots(directory: &Path) -> BTreeSet<PathBuf> {
    let mut roots = BTreeSet::new();
    let Some(manifest) = std::fs::read_to_string(directory.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
    else {
        return roots;
    };
    let Some(package) = manifest.get("package").filter(|package| package.is_table()) else {
        return roots;
    };
    let mut add = |path: PathBuf| {
        if path.is_file() {
            roots.insert(normalize(&path));
        }
    };
    for (table, target_directory, harness_off_only) in [
        ("bin", "src/bin", false),
        ("example", "examples", false),
        ("test", "tests", true),
        ("bench", "benches", true),
    ] {
        let targets = manifest
            .get(table)
            .and_then(toml::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for target in targets {
            // With the test harness on, the harness supplies `main`.
            if harness_off_only
                && target.get("harness").and_then(toml::Value::as_bool) != Some(false)
            {
                continue;
            }
            if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
                add(directory.join(path));
            } else if let Some(name) = target.get("name").and_then(toml::Value::as_str) {
                add(directory.join(target_directory).join(format!("{name}.rs")));
                add(directory.join(target_directory).join(name).join("main.rs"));
                if table == "bin" {
                    add(directory.join("src/main.rs"));
                }
            }
        }
    }
    let discover = |key: &str| {
        package
            .get(key)
            .and_then(toml::Value::as_bool)
            .unwrap_or(true)
    };
    let mut discovered = Vec::new();
    if discover("autobins") {
        discovered.push(directory.join("src/main.rs"));
        discovered.extend(target_files(&directory.join("src/bin")));
    }
    if discover("autoexamples") {
        discovered.extend(target_files(&directory.join("examples")));
    }
    match package.get("build") {
        Some(toml::Value::String(path)) => discovered.push(directory.join(path)),
        Some(toml::Value::Boolean(false)) => {}
        _ => discovered.push(directory.join("build.rs")),
    }
    for path in discovered {
        add(path);
    }
    roots
}

/// The target root files Cargo discovers in `directory`: each `*.rs` file,
/// and `main.rs` in each subdirectory.
fn target_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                Some(path.join("main.rs"))
            } else {
                (path.extension().and_then(|extension| extension.to_str()) == Some("rs"))
                    .then_some(path)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::callgraph_store_adapter::{
        impact_result, trace_to_result, StoreTraceToResult,
    };
    use crate::data_layer::query::{MemorySlices, SliceReader};

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
