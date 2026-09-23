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

