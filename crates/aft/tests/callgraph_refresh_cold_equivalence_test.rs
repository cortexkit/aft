//! A callgraph store refreshed from commit A to commit B must hold exactly the
//! graph a cold build of B produces. Callers, impact, trace and dead-code all
//! read these rows, so any difference means query answers depend on how the
//! store happened to be built.
//!
//! Every scenario builds its fixture repository in a temporary directory and
//! keeps both stores outside it, so the tests need no git checkout and no
//! writable project storage.

use aft::callgraph::walk_project_files;
use aft::callgraph_store::CallGraphStore;
use filetime::FileTime;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use tempfile::tempdir;

static NEXT_MTIME: AtomicI64 = AtomicI64::new(1_800_000_000);

/// One reference row as resolution leaves it. `ref_id` already encodes the
/// caller file, the site and the callee text, so equal ids name the same site.
type RefState = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// One `edges` row: id, ref, source node, target node, target file, target
/// symbol, kind, line and provenance. Unlike `StoredEdge` this keeps the target
/// node id, which callers/impact follow and which goes stale when the target
/// file's symbols move.
type EdgeRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    i64,
    String,
);

/// Everything resolution writes: the edge rows (including method-dispatch
/// edges and their target node ids), every ref's resolution state, and the
/// per-file dependency rows a later refresh uses to find a change's dependents.
#[derive(Debug)]
struct Snapshot {
    edges: BTreeSet<EdgeRow>,
    refs: BTreeSet<RefState>,
    dependencies: BTreeSet<(String, String)>,
}

fn write_file(root: &Path, rel: &str, content: &str) -> PathBuf {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    // Distinct mtimes keep the refresh freshness check from treating a rewrite
    // inside the same filesystem tick as unchanged.
    let mtime = FileTime::from_unix_time(NEXT_MTIME.fetch_add(1, Ordering::SeqCst), 0);
    filetime::set_file_mtime(&path, mtime).unwrap();
    path
}

fn snapshot(store: &CallGraphStore) -> Snapshot {
    let conn = Connection::open(store.sqlite_path()).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT edge_id, ref_id, source_node, target_node, target_file, target_symbol,
                    kind, line, provenance
             FROM edges",
        )
        .unwrap();
    let edges = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<EdgeRow>>>()
        .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT ref_id, kind, status, target_file, target_symbol, target_node
             FROM refs",
        )
        .unwrap();
    let refs = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<RefState>>>()
        .unwrap();
    let mut stmt = conn
        .prepare("SELECT file_path, dep_file FROM file_dependencies")
        .unwrap();
    let dependencies = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<(String, String)>>>()
        .unwrap();
    Snapshot {
        edges,
        refs,
        dependencies,
    }
}

fn cold_snapshot(root: &Path, store_dir: &Path) -> Snapshot {
    let store = CallGraphStore::open(store_dir.to_path_buf(), root.to_path_buf()).unwrap();
    let files: Vec<PathBuf> = walk_project_files(root).collect();
    store.cold_build(&files).unwrap();
    snapshot(&store)
}

/// Fails unless `refreshed` holds exactly the refs, edges and dependency rows
/// of `cold`.
fn assert_same_graph(name: &str, refreshed: &Snapshot, cold: &Snapshot) {
    let only_refreshed: Vec<_> = refreshed.refs.difference(&cold.refs).collect();
    let only_cold: Vec<_> = cold.refs.difference(&refreshed.refs).collect();
    assert!(
        only_refreshed.is_empty() && only_cold.is_empty(),
        "scenario {name}: ref states differ\nonly after refresh: {only_refreshed:#?}\nonly in cold build: {only_cold:#?}"
    );
    let edges_only_refreshed: Vec<_> = refreshed.edges.difference(&cold.edges).collect();
    let edges_only_cold: Vec<_> = cold.edges.difference(&refreshed.edges).collect();
    assert!(
        edges_only_refreshed.is_empty() && edges_only_cold.is_empty(),
        "scenario {name}: edges differ\nonly after refresh: {edges_only_refreshed:#?}\nonly in cold build: {edges_only_cold:#?}"
    );
    let deps_only_refreshed: Vec<_> = refreshed
        .dependencies
        .difference(&cold.dependencies)
        .collect();
    let deps_only_cold: Vec<_> = cold
        .dependencies
        .difference(&refreshed.dependencies)
        .collect();
    assert!(
        deps_only_refreshed.is_empty() && deps_only_cold.is_empty(),
        "scenario {name}: file dependencies differ\nonly after refresh: {deps_only_refreshed:#?}\nonly in cold build: {deps_only_cold:#?}"
    );
}

type Step<'a> = &'a [(&'a str, Option<&'a str>)];

/// Builds `before` cold, then applies each step (a `None` content deletes the
/// file) and refreshes exactly the paths that step touched, and finally
/// compares the store with a cold build of the resulting tree. Returns the
/// cold snapshot so a scenario can also pin what the right answer is.
fn assert_refresh_matches_cold(
    name: &str,
    before: &[(&str, &str)],
    steps: &[Step<'_>],
) -> Snapshot {
    let project = tempdir().unwrap();
    let root = fs::canonicalize(project.path()).unwrap();
    let stores = tempdir().unwrap();
    for (rel, content) in before {
        write_file(&root, rel, content);
    }

    let store =
        CallGraphStore::open(stores.path().join("incremental"), root.to_path_buf()).unwrap();
    let files: Vec<PathBuf> = walk_project_files(&root).collect();
    store.cold_build(&files).unwrap();

    for step in steps {
        let mut changed = Vec::new();
        for (rel, content) in *step {
            match content {
                Some(content) => changed.push(write_file(&root, rel, content)),
                None => {
                    let path = root.join(rel);
                    fs::remove_file(&path).unwrap();
                    changed.push(path);
                }
            }
        }
        store.refresh_files(&changed).unwrap();
    }
    let refreshed = snapshot(&store);
    drop(store);

    let cold = cold_snapshot(&root, &stores.path().join("cold"));
    assert_same_graph(name, &refreshed, &cold);
    cold
}

/// `(target_file, target_symbol, provenance)` of every edge into `symbol`.
fn edges_to(snapshot: &Snapshot, symbol: &str) -> Vec<(String, String, String)> {
    snapshot
        .edges
        .iter()
        .filter(|edge| edge.5 == symbol)
        .map(|edge| (edge.4.clone(), edge.5.clone(), edge.8.clone()))
        .collect()
}

const WIDGET_TEST: &str = r#"import { describe, expect, it } from "bun:test";
import { render } from "../src/widget";

describe("widget", () => {
  it("renders", () => {
    expect(render()).toBe(1);
  });
});
"#;

/// `describe`/`it`/`expect` come from `bun:test`, which no indexed file
/// provides. The cold build used to fall back to the importer's first
/// dependency file (here `src/widget.ts`) for any module it could not place,
/// so test-framework globals "resolved" to an unrelated source file.
#[test]
fn external_module_imports_stay_unresolved() {
    let cold = assert_refresh_matches_cold(
        "test globals from an external module",
        &[
            ("src/widget.ts", "export function render() { return 1; }\n"),
            ("test/widget.test.ts", WIDGET_TEST),
        ],
        &[&[(
            "src/widget.ts",
            Some("export function render() { return 1; }\nexport function extra() {}\n"),
        )]],
    );
    for symbol in ["describe", "it", "expect"] {
        assert!(
            edges_to(&cold, symbol).is_empty(),
            "{symbol} must not resolve to a local file: {:#?}",
            edges_to(&cold, symbol)
        );
    }
    assert_eq!(
        edges_to(&cold, "render"),
        vec![(
            "src/widget.ts".to_string(),
            "render".to_string(),
            "treesitter+resolver".to_string()
        )]
    );
}

/// An import the resolver places on a file the store does not index (an image
/// here) has no target. A refresh used to take the first existing file of the
/// import's dependency set, indexed or not.
#[test]
fn import_of_unindexed_file_stays_unresolved() {
    let cold = assert_refresh_matches_cold(
        "import of an unindexed file",
        &[
            ("logo.png", "not really a png\n"),
            ("helper.ts", "export function helper() {}\n"),
            (
                "main.ts",
                "import logo from \"./logo.png\";\nimport { helper } from \"./helper\";\nexport function main() { logo(); helper(); }\n",
            ),
        ],
        &[&[(
            "helper.ts",
            Some("export function helper() {}\nexport function more() {}\n"),
        )]],
    );
    assert!(edges_to(&cold, "<default:logo.png>").is_empty());
    assert_eq!(edges_to(&cold, "helper").len(), 1);
}

/// With both `foo.js` and `foo.ts` present, `./foo` is `foo.ts` (the resolver
/// tries TypeScript first). A refresh used to take the alphabetically first
/// existing candidate, `foo.js`.
#[test]
fn relative_import_uses_resolver_precedence() {
    let cold = assert_refresh_matches_cold(
        "relative import with two candidate files",
        &[
            ("foo.js", "export function run() {}\n"),
            ("foo.ts", "export function run() {}\n"),
            (
                "main.ts",
                "import { run } from \"./foo\";\nexport function main() { run(); }\n",
            ),
        ],
        &[&[(
            "main.ts",
            Some("import { run } from \"./foo\";\nexport function main() { run(); run(); }\n"),
        )]],
    );
    let targets = edges_to(&cold, "run");
    assert_eq!(targets.len(), 2, "{targets:#?}");
    assert!(
        targets.iter().all(|target| target.0 == "foo.ts"),
        "{targets:#?}"
    );
}

const WORKSPACE: &[(&str, &str)] = &[
    (
        "package.json",
        "{\"name\":\"root\",\"private\":true,\"workspaces\":[\"packages/*\"]}\n",
    ),
    (
        "packages/core/package.json",
        "{\"name\":\"@s/core\",\"main\":\"src/index.ts\"}\n",
    ),
    ("packages/core/src/index.ts", "export * from \"./run\";\n"),
    ("packages/core/src/run.ts", "export function run() {}\n"),
    (
        "packages/app/package.json",
        "{\"name\":\"@s/app\",\"main\":\"src/index.ts\"}\n",
    ),
    ("packages/app/src/index.ts", "export * from \"@s/core\";\n"),
    (
        "packages/app/src/main.ts",
        "import { run } from \"./index\";\nexport function main() { run(); }\n",
    ),
];

/// A re-export of a workspace package (`export * from "@s/core"`) in a file the
/// refresh does not rewrite. The refresh guessed its target from the file's
/// dependency set by package-name matching and lost it when two dependencies
/// matched; the cold build asked the resolver.
#[test]
fn workspace_package_reexport_in_unchanged_barrel() {
    let cold = assert_refresh_matches_cold(
        "workspace package re-export",
        WORKSPACE,
        &[&[(
            "packages/core/src/run.ts",
            Some("export function run() {}\nexport function more() {}\n"),
        )]],
    );
    assert_eq!(
        edges_to(&cold, "run"),
        vec![(
            "packages/core/src/run.ts".to_string(),
            "run".to_string(),
            "treesitter+resolver".to_string()
        )]
    );
}

/// `export *` re-exports are searched in source order. A refresh read a
/// barrel's persisted re-export rows in rowid order, and a rewrite that keeps
/// one row and replaces an earlier one puts the replacement after it.
#[test]
fn reexport_search_order_is_source_order() {
    let cold = assert_refresh_matches_cold(
        "re-export order after a partial rewrite",
        &[
            (
                "main.ts",
                "import { foo } from \"./barrel\";\nexport function main() { foo(); }\n",
            ),
            (
                "barrel.ts",
                "export * from \"./zz\";\nexport * from \"./aa\";\n",
            ),
            ("aa.ts", "export function foo() {}\n"),
            ("bb.ts", "export function foo() {}\n"),
        ],
        &[
            // Same byte ranges, new module path: the second row keeps its id.
            &[(
                "barrel.ts",
                Some("export * from \"./bb\";\nexport * from \"./aa\";\n"),
            )],
            // Re-resolve main.ts while barrel.ts is read from the database.
            &[(
                "main.ts",
                Some(
                    "import { foo } from \"./barrel\";\nexport function main() { foo(); foo(); }\n",
                ),
            )],
        ],
    );
    assert!(
        edges_to(&cold, "foo")
            .iter()
            .all(|target| target.0 == "bb.ts"),
        "{:#?}",
        edges_to(&cold, "foo")
    );
}

/// Several names in one `export { ... }` list. Their refs used to be numbered
/// in hash-map order, so two extractions of the same file gave different ref
/// ids and a different surface fingerprint.
#[test]
fn export_alias_rows_are_deterministic() {
    assert_refresh_matches_cold(
        "export alias list",
        &[
            (
                "lib.ts",
                "function a() {}\nfunction b() {}\nfunction c() {}\nfunction d() {}\nfunction e() {}\nfunction f() {}\nexport { a as one, b as two, c as three, d as four, e as five, f as six };\n",
            ),
            (
                "main.ts",
                "import { one } from \"./lib\";\nexport function main() { one(); }\n",
            ),
        ],
        &[&[(
            "main.ts",
            Some("import { one } from \"./lib\";\nexport function main() { one(); one(); }\n"),
        )]],
    );
}

/// `main.ts` imports `foo` through a barrel that re-exports `impl.ts`. When
/// `impl.ts` starts exporting `foo`, the call must move to it even though
/// `main.ts` never imports `impl.ts` directly.
#[test]
fn transitive_reexport_consumers_are_refreshed() {
    let cold = assert_refresh_matches_cold(
        "export added behind a barrel",
        &[
            (
                "main.ts",
                "import { foo } from \"./barrel\";\nexport function main() { foo(); }\n",
            ),
            ("barrel.ts", "export * from \"./mid\";\n"),
            (
                "mid.ts",
                "export { foo as foo } from \"./impl\";\nexport * from \"./impl\";\n",
            ),
            ("impl.ts", "export function bar() {}\n"),
        ],
        &[&[(
            "impl.ts",
            Some("export function bar() {}\nexport function foo() {}\n"),
        )]],
    );
    assert!(
        edges_to(&cold, "foo")
            .iter()
            .all(|target| target.0 == "impl.ts"),
        "{:#?}",
        edges_to(&cold, "foo")
    );
}

/// The reverse: the export disappears behind the barrel.
#[test]
fn transitive_reexport_removal_is_refreshed() {
    assert_refresh_matches_cold(
        "export removed behind a barrel",
        &[
            (
                "main.ts",
                "import { foo } from \"./barrel\";\nexport function main() { foo(); }\n",
            ),
            ("barrel.ts", "export * from \"./impl\";\n"),
            ("impl.ts", "export function foo() {}\n"),
            ("other.ts", "export function foo() {}\n"),
        ],
        &[&[("impl.ts", Some("export function bar() {}\n"))]],
    );
}

/// `@/late` does not resolve until `src/late.ts` exists, so the importer's
/// dependency set could not name it. Creating the file must still re-resolve
/// the importer.
#[test]
fn created_file_satisfies_path_alias_import() {
    let cold = assert_refresh_matches_cold(
        "path alias target created",
        &[
            (
                "tsconfig.json",
                "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"@/*\":[\"src/*\"]}}}\n",
            ),
            (
                "src/main.ts",
                "import { late } from \"@/late\";\nexport function main() { late(); }\n",
            ),
            ("src/other.ts", "export function other() {}\n"),
        ],
        &[&[("src/late.ts", Some("export function late() {}\n"))]],
    );
    assert_eq!(edges_to(&cold, "late").len(), 1);
}

/// The same for a TypeScript ESM specifier that names the emitted `.js` file.
#[test]
fn created_file_satisfies_js_extension_import() {
    let cold = assert_refresh_matches_cold(
        "ts file created for a .js specifier",
        &[(
            "main.ts",
            "import { late } from \"./late.js\";\nexport function main() { late(); }\n",
        )],
        &[&[("late.ts", Some("export function late() {}\n"))]],
    );
    assert_eq!(edges_to(&cold, "late").len(), 1);
}

/// A method-dispatch edge records the target method's node id. Moving the
/// method inside its file changes that id, and callers in untouched files must
/// follow it.
#[test]
fn dispatch_edges_follow_moved_target_methods() {
    let cold = assert_refresh_matches_cold(
        "dispatch target moved",
        &[
            ("store.ts", "export class Store {\n  save() {}\n}\n"),
            ("main.ts", "export function main(s: any) { s.save(); }\n"),
        ],
        &[&[(
            "store.ts",
            Some("// moved down\nexport class Store {\n  save() {}\n}\n"),
        )]],
    );
    assert!(
        !edges_to(&cold, "Store::save").is_empty() || !edges_to(&cold, "Store.save").is_empty(),
        "{:#?}",
        cold.edges
    );
}

/// A new method with the same name changes which candidate name matching
/// picks (or makes the call ambiguous) for callers in untouched files.
#[test]
fn dispatch_edges_follow_new_candidate_methods() {
    assert_refresh_matches_cold(
        "dispatch candidate added",
        &[
            ("a/store.ts", "export class Store {\n  save() {}\n}\n"),
            ("b/main.ts", "export function main(s: any) { s.save(); }\n"),
        ],
        &[&[("b/cache.ts", Some("export class Cache {\n  save() {}\n}\n"))]],
    );
}

/// A call re-resolved in a caller the refresh does not rewrite can switch
/// between a direct edge and a method-dispatch edge. `store.save()` binds by
/// name to `lib.ts`'s exported `save` while there is one, and falls back to
/// name matching (`Keeper::save`) when it goes away; both switches must be
/// reflected.
#[test]
fn dispatch_edges_follow_dependent_status_changes() {
    let with_save = "export function save() {}\nexport const store = { go() {} };\n";
    let without_save = "export function keep() {}\nexport const store = { go() {} };\n";
    let main = "import { store } from \"./lib\";\nexport function main() { store.save(); }\n";
    let fixture: &[(&str, &str)] = &[
        ("lib.ts", with_save),
        ("other.ts", "export class Keeper {\n  save() {}\n}\n"),
        ("main.ts", main),
    ];
    let cold = assert_refresh_matches_cold(
        "dependent call becomes a dispatch call",
        fixture,
        &[&[("lib.ts", Some(without_save))]],
    );
    assert!(
        edges_to(&cold, "Keeper::save")
            .iter()
            .any(|edge| edge.2 == "name_match"),
        "{:#?}",
        cold.edges
    );
    assert_refresh_matches_cold(
        "dispatch call becomes a direct call again",
        fixture,
        &[
            &[("lib.ts", Some(without_save))],
            &[("lib.ts", Some(with_save))],
        ],
    );
}

/// A Rust value reference (a function passed as a value) resolves only when
/// its target is callable. It must be re-resolved when the target changes.
#[test]
fn rust_value_refs_are_refreshed_with_their_target() {
    assert_refresh_matches_cold(
        "rust value ref target becomes callable",
        &[
            ("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"),
            ("src/lib.rs", "mod app;\nmod util;\n"),
            ("src/util.rs", "pub const HELPER: u32 = 1;\n"),
            (
                "src/app.rs",
                "use crate::util::HELPER;\npub fn run() -> Vec<u32> {\n    vec![1u32].into_iter().map(HELPER).collect()\n}\n",
            ),
        ],
        &[&[(
            "src/util.rs",
            Some("#[allow(non_snake_case)]\npub fn HELPER(x: u32) -> u32 {\n    x\n}\n"),
        )]],
    );
}

const RUST_REEXPORT_CRATE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    ),
    ("src/lib.rs", "pub mod git;\nmod app;\n"),
    ("src/git/mod.rs", "mod cli;\npub use cli::{clone, other};\n"),
    ("src/git/cli.rs", "pub fn other() {}\n"),
    (
        "src/app.rs",
        "pub fn run() {\n    crate::git::clone();\n    crate::git::other();\n}\n",
    ),
];

/// `pub use cli::{..}` in `git/mod.rs` names the module it declares with
/// `mod cli;` (`git/cli.rs`). When `clone` appears there, the call in `app.rs`,
/// which only reaches it through that re-export, must follow.
#[test]
fn rust_pub_use_of_declared_module_is_refreshed() {
    let cold = assert_refresh_matches_cold(
        "rust re-export of a declared module gains an item",
        RUST_REEXPORT_CRATE,
        &[&[(
            "src/git/cli.rs",
            Some("pub fn other() {}\npub fn clone() {}\n"),
        )]],
    );
    assert!(
        edges_to(&cold, "clone")
            .iter()
            .all(|target| target.0 == "src/git/cli.rs"),
        "{:#?}",
        edges_to(&cold, "clone")
    );
    assert!(!edges_to(&cold, "clone").is_empty(), "{:#?}", cold.edges);
}

/// A created `index.ts` answers an existing import of its directory.
#[test]
fn created_file_satisfies_directory_index_import() {
    let cold = assert_refresh_matches_cold(
        "index file created for a directory import",
        &[
            (
                "src/main.ts",
                "import { widget } from \"./widgets\";\nexport function main() { widget(); }\n",
            ),
            ("src/widgets/button.ts", "export function button() {}\n"),
        ],
        &[&[(
            "src/widgets/index.ts",
            Some("export function widget() {}\n"),
        )]],
    );
    assert_eq!(edges_to(&cold, "widget").len(), 1);
}

/// An exact `paths` alias (no wildcard) names a fixed file, so nothing in the
/// specifier resembles the created file's name.
#[test]
fn created_file_satisfies_exact_path_alias() {
    let cold = assert_refresh_matches_cold(
        "exact path alias target created",
        &[
            (
                "tsconfig.json",
                "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"@settings\":[\"src/config/values.ts\"]}}}\n",
            ),
            (
                "src/main.ts",
                "import { load } from \"@settings\";\nexport function main() { load(); }\n",
            ),
        ],
        &[&[("src/config/values.ts", Some("export function load() {}\n"))]],
    );
    assert_eq!(edges_to(&cold, "load").len(), 1);
}

/// A workspace package whose entry file is created after its importers.
#[test]
fn created_file_satisfies_workspace_package_entry() {
    let cold = assert_refresh_matches_cold(
        "workspace package entry created",
        &[
            (
                "package.json",
                "{\"name\":\"root\",\"private\":true,\"workspaces\":[\"packages/*\"]}\n",
            ),
            (
                "packages/core/package.json",
                "{\"name\":\"@s/core\",\"main\":\"lib/entry.ts\"}\n",
            ),
            ("packages/core/lib/other.ts", "export function other() {}\n"),
            (
                "packages/app/src/main.ts",
                "import { run } from \"@s/core\";\nexport function main() { run(); }\n",
            ),
        ],
        &[&[(
            "packages/core/lib/entry.ts",
            Some("export function run() {}\n"),
        )]],
    );
    assert_eq!(edges_to(&cold, "run").len(), 1);
}

/// A created Rust file answers an existing `mod` declaration; the declaring
/// file's dependency rows must name it, as a cold build's do.
#[test]
fn created_file_satisfies_rust_mod_declaration() {
    assert_refresh_matches_cold(
        "rust module file created",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/lib.rs",
                "mod util;\npub fn run() {\n    util::helper();\n}\n",
            ),
        ],
        &[&[("src/util.rs", Some("pub fn helper() {}\n"))]],
    );
}

/// A created file nothing refers to: the refresh must still match the cold
/// build (and should not need to re-resolve anything else).
#[test]
fn created_file_without_importers() {
    assert_refresh_matches_cold(
        "unreferenced file created",
        &[
            (
                "src/main.ts",
                "import { helper } from \"./helper\";\nexport function main() { helper(); }\n",
            ),
            ("src/helper.ts", "export function helper() {}\n"),
        ],
        &[&[("src/fresh.ts", Some("export function fresh() {}\n"))]],
    );
}

/// Rust stores a path call's whole path as its short name (`Widget::build`),
/// so a method added in another file must still reach that caller's dispatch
/// edge.
#[test]
fn dispatch_edges_follow_new_rust_path_candidates() {
    let cold = assert_refresh_matches_cold(
        "rust method added for a path call",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "mod app;\nmod shapes;\n"),
            (
                "src/shapes.rs",
                "pub struct Widget;\nimpl Widget {\n    pub fn other() {}\n}\n",
            ),
            (
                "src/app.rs",
                "pub fn run() {\n    Widget::build();\n}\n",
            ),
        ],
        &[&[(
            "src/shapes.rs",
            Some("pub struct Widget;\nimpl Widget {\n    pub fn other() {}\n    pub fn build() {}\n}\n"),
        )]],
    );
    assert!(
        !edges_to(&cold, "Widget::build").is_empty(),
        "{:#?}",
        cold.edges
    );
}

/// Unrelated stored files, each a small re-export chain of its own. A one-file
/// refresh elsewhere must not need to read any of them.
fn padding_files() -> Vec<(String, String)> {
    (0..30)
        .map(|index| {
            let next = if index < 29 {
                format!("export * from \"./pad{}\";\n", index + 1)
            } else {
                String::new()
            };
            (
                format!("pad{index}.ts"),
                format!("{next}export function pad{index}() {{}}\n"),
            )
        })
        .collect()
}

/// A one-file edit whose call resolves through a three-hop re-export chain
/// (`export *`, a named re-export, `export *` again) into files the batch does
/// not touch. The refresh loads stored file indexes on demand: it must reach
/// the same target a cold build does while loading only the four files on
/// that chain, not the whole store.
#[test]
fn one_file_edit_resolves_through_untouched_reexport_chain_loading_only_that_chain() {
    let project = tempdir().unwrap();
    let root = fs::canonicalize(project.path()).unwrap();
    let stores = tempdir().unwrap();
    let mut before = vec![
        (
            "main.ts".to_string(),
            "import { foo } from \"./barrel\";\nexport function main() { foo(); }\n".to_string(),
        ),
        ("barrel.ts".to_string(), "export * from \"./mid\";\n".to_string()),
        (
            "mid.ts".to_string(),
            "export { foo } from \"./deep\";\n".to_string(),
        ),
        ("deep.ts".to_string(), "export * from \"./impl\";\n".to_string()),
        (
            "impl.ts".to_string(),
            "export function foo() {}\n".to_string(),
        ),
    ];
    before.extend(padding_files());
    for (rel, content) in &before {
        write_file(&root, rel, content);
    }
    let store =
        CallGraphStore::open(stores.path().join("incremental"), root.to_path_buf()).unwrap();
    let files: Vec<PathBuf> = walk_project_files(&root).collect();
    store.cold_build(&files).unwrap();

    let main = write_file(
        &root,
        "main.ts",
        "import { foo } from \"./barrel\";\nexport function main() { foo(); foo(); }\n",
    );
    let (stats, profile) = store.refresh_files_profiled(&[main]).unwrap();
    assert_eq!(stats.refreshed_own_files, 1, "{stats:?}");
    let refreshed = snapshot(&store);
    drop(store);

    let cold = cold_snapshot(&root, &stores.path().join("cold"));
    assert_same_graph("one-file edit through a re-export chain", &refreshed, &cold);
    let targets = edges_to(&cold, "foo");
    assert_eq!(targets.len(), 2, "{targets:#?}");
    assert!(
        targets.iter().all(|target| target.0 == "impl.ts"),
        "{targets:#?}"
    );

    // barrel.ts, mid.ts, deep.ts and impl.ts; main.ts is re-resolved from its
    // new extract and none of the 30 padding files is on the chain.
    assert_eq!(profile.index_loads, 1, "{}", profile.report());
    assert_eq!(profile.index_files_loaded, 4, "{}", profile.report());
    // Their four `files` rows, one node, three re-export refs and a handful of
    // existence checks: far below the 30 padding files' rows alone.
    assert!(
        profile.index_rows_read <= 20,
        "a one-file refresh must not read the whole store: {}",
        profile.report()
    );
}

/// A Rust file registered through `mod` declarations in two stored files and
/// calling into an inline module of one of them. Resolving it walks the module
/// parents and searches inline modules across stored files, both of which
/// consult every indexed file rather than one import.
#[test]
fn rust_edit_resolves_through_stored_module_parents_and_inline_modules() {
    let cold = assert_refresh_matches_cold(
        "rust edit through stored module declarations",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub mod net;\nmod app;\n"),
            (
                "src/net/mod.rs",
                "pub mod http;\npub mod wire {\n    pub fn ping() {}\n}\n",
            ),
            (
                "src/net/http.rs",
                "pub fn get() {\n    super::wire::ping();\n}\n",
            ),
            ("src/app.rs", "pub fn run() {\n    crate::net::http::get();\n}\n"),
        ],
        &[&[(
            "src/net/http.rs",
            Some("pub fn get() {\n    super::wire::ping();\n    super::wire::ping();\n}\n"),
        )]],
    );
    assert_eq!(
        edges_to(&cold, "wire::ping")
            .into_iter()
            .map(|target| target.0)
            .collect::<Vec<_>>(),
        vec!["src/net/mod.rs".to_string(), "src/net/mod.rs".to_string()],
        "{:#?}",
        cold.edges
    );
}
