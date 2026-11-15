//! A cold build resolves method calls from records projected in file order
//! (`dispatch::projected`); the live engine resolves them reading files.
//! Each fixture exercises one rule the records must carry and checks that
//! both give the same resolution, recorded reads included.

use std::collections::BTreeMap;

use super::callgraph::{resolve_checkout_within, CheckoutInputs, FileResolution, KeyOrderBudget};
use super::incremental::LiveCheckout;
use super::incremental_tests::checkout;

/// Resolves `inputs` reading files, as the live engine does, and from
/// projected records in key order on 1 and 4 threads, checking every
/// file's resolution is equal. Besides the default budget, the key order
/// runs with a budget of one byte, where every key is resolved on its own,
/// every group with more than one key is shared by every thread, and the
/// sites of every key with more than one go through a scratch file; and
/// with a few keys per flush. Returns the file-reading resolution.
fn records_match_files(inputs: &CheckoutInputs) -> BTreeMap<String, FileResolution> {
    let reference = LiveCheckout::cold(inputs.clone())
        .unwrap()
        .resolved
        .to_map();
    let budgets = [
        KeyOrderBudget::default(),
        KeyOrderBudget::of_bytes(1),
        KeyOrderBudget::of_bytes(4096),
    ];
    for (threads, budget) in [1, 4]
        .into_iter()
        .flat_map(|threads| budgets.map(|budget| (threads, budget)))
    {
        let records = resolve_checkout_within(inputs, threads, budget).unwrap();
        assert_eq!(
            records.keys().collect::<Vec<_>>(),
            reference.keys().collect::<Vec<_>>()
        );
        for (path, ours) in &records {
            let theirs = &reference[path];
            assert_eq!(
                ours.sites, theirs.sites,
                "{path}: sites ({threads} threads, {budget:?})"
            );
            assert_eq!(
                ours.site_dependencies, theirs.site_dependencies,
                "{path}: site dependencies ({threads} threads, {budget:?})"
            );
            assert_eq!(ours, theirs, "{path} ({threads} threads, {budget:?})");
        }
    }
    reference
}

/// The (file, symbol, provenance) of every target of `path`'s sites calling
/// `member`.
fn targets(
    resolved: &BTreeMap<String, FileResolution>,
    path: &str,
    member: &str,
) -> Vec<(String, String, &'static str)> {
    resolved[path]
        .sites
        .iter()
        .filter(|site| site.site.member.as_deref() == Some(member))
        .flat_map(|site| &site.resolution.targets)
        .map(|target| {
            (
                target.file.clone(),
                target.symbol.clone(),
                target.provenance,
            )
        })
        .collect()
}

#[test]
fn a_typescript_receiver_declared_in_a_tsx_file_keeps_its_exact_edge() {
    let inputs = checkout(
        &[
            (
                "src/run.ts",
                "import { Widget } from \"./widget\";\nexport function run(w: Widget) {\n  w.render();\n}\n",
            ),
            (
                "src/widget.tsx",
                "export class Widget {\n  render() {\n    return 1;\n  }\n}\n",
            ),
            ("src/empty.ts", "export const value = 1;\n"),
        ],
        &[],
    );
    let resolved = records_match_files(&inputs);
    assert!(
        targets(&resolved, "src/run.ts", "render")
            .iter()
            .any(|(file, _, provenance)| file == "src/widget.tsx" && *provenance == "exact"),
        "{:?}",
        resolved["src/run.ts"].sites
    );
    // A file with no sites still depends on its own facts.
    assert_eq!(
        resolved["src/empty.ts"]
            .site_dependencies
            .iter()
            .collect::<Vec<_>>(),
        ["r:src/empty.ts"]
    );
}

#[test]
fn python_super_follows_a_diamond_across_files_and_reused_ordinals() {
    let inputs = checkout(
        &[
            (
                "pkg/__init__.py",
                "",
            ),
            (
                "pkg/base.py",
                "class A:\n    def hello(self):\n        return 1\n\n\nclass B(A):\n    def other(self):\n        return 0\n\n\nclass C(A):\n    def hello(self):\n        return super().hello()\n",
            ),
            (
                "pkg/leaf.py",
                "from pkg.base import B, C\n\n\nclass D(B, C):\n    def hello(self):\n        return super().hello()\n\n\ndef outer(item):\n    def inner():\n        return item.hello()\n    return inner() + item.hello()\n",
            ),
        ],
        &[],
    );
    // Python keeps a site for each caller a nested call has, under one
    // syntax ordinal; the records key sites by position, not ordinal.
    let leaf = inputs.parses.read("pkg/leaf.py").unwrap().1;
    let ordinals = leaf
        .dispatch
        .sites
        .iter()
        .map(|site| site.ordinal)
        .collect::<Vec<_>>();
    let distinct = ordinals.iter().collect::<std::collections::BTreeSet<_>>();
    assert!(distinct.len() < ordinals.len(), "{:?}", leaf.dispatch.sites);
    let resolved = records_match_files(&inputs);
    // D's resolution order is D, B, C, A: B declares no `hello`, so D's
    // super() reaches C's, not A's.
    assert!(
        targets(&resolved, "pkg/leaf.py", "hello")
            .iter()
            .any(|(file, symbol, _)| file == "pkg/base.py" && symbol == "C::hello"),
        "{:?}",
        resolved["pkg/leaf.py"].sites
    );
    // C's own super() reaches A.
    assert!(targets(&resolved, "pkg/base.py", "hello")
        .iter()
        .any(|(_, symbol, _)| symbol == "A::hello"));
}

#[test]
fn rust_associated_calls_trait_checks_and_dropped_ordinary_edges_match() {
    let inputs = checkout(
        &[
            ("app/Cargo.toml", "[package]\nname = \"app\"\n"),
            (
                "app/src/lib.rs",
                "pub mod area;\npub mod other;\npub mod shapes;\n\nuse crate::area::Area;\nuse crate::shapes::Circle;\n\npub fn run(c: Circle) -> i32 {\n    let made = Circle::new();\n    shapes::make();\n    made.radius() + c.area()\n}\n",
            ),
            (
                "app/src/shapes.rs",
                "pub struct Circle {\n    r: i32,\n}\n\nimpl Circle {\n    pub fn new() -> Self {\n        Circle { r: 1 }\n    }\n    pub fn radius(&self) -> i32 {\n        self.r\n    }\n}\n\npub fn make() -> Circle {\n    Circle::new()\n}\n",
            ),
            (
                "app/src/area.rs",
                "use crate::shapes::Circle;\n\npub trait Area {\n    fn area(&self) -> i32;\n}\n\nimpl Area for Circle {\n    fn area(&self) -> i32 {\n        2\n    }\n}\n",
            ),
            (
                // Calls the trait method without importing the trait: the
                // trait check asks this file for a name it cannot resolve.
                "app/src/other.rs",
                "use crate::shapes::Circle;\n\npub fn go(c: Circle) -> i32 {\n    c.area()\n}\n",
            ),
        ],
        &[],
    );
    let resolved = records_match_files(&inputs);
    assert!(targets(&resolved, "app/src/lib.rs", "new")
        .iter()
        .any(|(file, _, _)| file == "app/src/shapes.rs"));
    assert!(
        resolved["app/src/other.rs"]
            .site_dependencies
            .contains("t:app/src/other.rs\tArea"),
        "{:?}",
        resolved["app/src/other.rs"].site_dependencies
    );
}

#[test]
fn go_interface_dispatch_compares_whole_method_sets() {
    let reader = "package a\n\ntype Reader interface {\n\tRead() int\n\tClose() int\n}\n\nfunc Use(r Reader) int {\n\treturn r.Read()\n}\n";
    let half = "package a\n\ntype Half struct{}\n\nfunc (h Half) Read() int { return 1 }\n";
    let full = "package a\n\ntype File struct{}\n\nfunc (f File) Read() int { return 1 }\n\nfunc (f File) Close() int { return 0 }\n";
    let without_close =
        "package a\n\ntype File struct{}\n\nfunc (f File) Read() int { return 1 }\n";
    let inputs = checkout(
        &[
            ("a/reader.go", reader),
            ("a/half.go", half),
            ("a/file.go", full),
        ],
        &[],
    );
    let resolved = records_match_files(&inputs);
    let found = targets(&resolved, "a/reader.go", "Read");
    assert!(
        found.iter().any(|(file, _, _)| file == "a/file.go"),
        "{found:?}"
    );
    assert!(
        !found.iter().any(|(file, _, _)| file == "a/half.go"),
        "{found:?}"
    );

    // Removing `Close` changes the dispatch of `Read`: a refresh must see it
    // and give what a cold build from records gives.
    let after = checkout(
        &[
            ("a/reader.go", reader),
            ("a/half.go", half),
            ("a/file.go", without_close),
        ],
        &[],
    );
    let mut live = LiveCheckout::cold(inputs).unwrap();
    live.update(after.clone(), &["a/file.go".to_string()].into())
        .unwrap();
    let cold = records_match_files(&after);
    assert!(!targets(&cold, "a/reader.go", "Read")
        .iter()
        .any(|(file, _, _)| file == "a/file.go"));
    for (path, ours) in live.resolved.iter() {
        assert_eq!(ours, &cold[path], "{path}");
    }
}

#[test]
fn cpp_type_matches_leave_the_caller_out() {
    let inputs = checkout(
        &[(
            "src/node.cpp",
            "struct Node {\n  void visit();\n};\n\nstruct Other {\n  void visit();\n};\n\nvoid Node::visit() {\n  Node n;\n  n.visit();\n}\n\nvoid Other::visit() {\n  Node n;\n  n.visit();\n}\n",
        )],
        &[],
    );
    records_match_files(&inputs);
}

#[test]
fn dynamic_sites_and_directory_scans_match() {
    let inputs = checkout(
        &[
            (
                "web/call.js",
                "export function call(target, key) {\n  return target[key]();\n}\n",
            ),
            (
                "java/Foo.java",
                "class Foo {\n  int bar() { return 1; }\n}\n",
            ),
            (
                "java/Main.java",
                "class Main {\n  int run(Foo f) { return f.bar(); }\n}\n",
            ),
        ],
        &[],
    );
    let resolved = records_match_files(&inputs);
    assert!(resolved["web/call.js"]
        .sites
        .iter()
        .any(|site| site.resolution.dynamic == 1));
    assert!(targets(&resolved, "java/Main.java", "bar")
        .iter()
        .any(|(file, _, _)| file == "java/Foo.java"));
}

/// One untyped call made from many files, twice in each, and declared in
/// a TypeScript and a JavaScript file: one key per language, its sites in
/// many files, each file depending on what the key's resolution read.
#[test]
fn an_untyped_call_from_many_files_is_resolved_once_per_language() {
    let callers = (0..12)
        .map(|index| {
            (
                format!("src/caller{index:02}.ts"),
                format!("export function run{index}(x) {{\n  x.dispose();\n  x.dispose();\n}}\n"),
            )
        })
        .collect::<Vec<_>>();
    let mut files = callers
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect::<Vec<_>>();
    files.extend([
        (
            "src/a.ts",
            "export interface Disposable {\n  dispose(): void;\n}\nexport class A implements Disposable {\n  dispose() {}\n}\n",
        ),
        ("lib/b.js", "export class B {\n  dispose() {}\n}\n"),
        ("lib/use.js", "export function use(y) {\n  y.dispose();\n}\n"),
    ]);
    let resolved = records_match_files(&super::incremental_tests::checkout(&files, &[]));
    for (path, _) in &callers {
        assert_eq!(resolved[path].sites.len(), 2, "{path}");
        assert!(
            resolved[path].site_dependencies.contains("m:dispose"),
            "{path}: {:?}",
            resolved[path].site_dependencies
        );
    }
    let javascript = &resolved["lib/use.js"];
    assert_eq!(javascript.sites.len(), 1);
    assert_eq!(javascript.sites[0].site.member.as_deref(), Some("dispose"));
    assert!(javascript.site_dependencies.contains("m:dispose"));
    // An untyped JavaScript receiver protects methods without claiming a callee.
    let answer = &javascript.sites[0].resolution;
    assert!(answer.targets.is_empty());
    assert!(!answer.protected.is_empty());
    assert!(answer.protected.iter().all(|(file, _)| file == "lib/b.js"));
}

/// The same comparison on a real tree, named by `AFT_SITE_RECORDS_ROOT`:
/// every file's resolution from records against reading files.
#[test]
#[ignore = "needs AFT_SITE_RECORDS_ROOT naming a checkout"]
fn records_match_files_on_a_real_tree() {
    let root = std::env::var("AFT_SITE_RECORDS_ROOT").unwrap();
    let built = super::trace_measure::build(std::path::Path::new(&root), 8).unwrap();
    let reference = LiveCheckout::cold(built.inputs.clone())
        .unwrap()
        .resolved
        .to_map();
    assert_eq!(
        built.resolved.keys().collect::<Vec<_>>(),
        reference.keys().collect::<Vec<_>>()
    );
    let mut differing = Vec::new();
    let mut sites = 0;
    for (path, ours) in &built.resolved {
        sites += ours.sites.len();
        if ours != &reference[path]
            || super::fragment::encode(ours) != super::fragment::encode(&reference[path])
        {
            differing.push(path.clone());
        }
    }
    println!(
        "records_match_files_on_a_real_tree: {} files, {sites} kept sites, {} differing {:?}",
        built.resolved.len(),
        differing.len(),
        differing.iter().take(20).collect::<Vec<_>>()
    );
    assert!(differing.is_empty());
}
