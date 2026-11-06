use super::*;
use std::fs;

pub(crate) const HUB: &str = "pub struct Hub;\nimpl Hub { pub fn ping(&self) {} pub fn indirect_ping(&self) {} }\npub fn target(_: ()) {}\npub fn own_call(x: &Hub) { x.ping(); }\npub trait I { fn uncommon_s4(&self) {} }\n";
pub(crate) const CALLER: &str = "use crate::hub::{Hub, I, target};\npub fn caller(x: &Hub) { target(()); std::iter::once(()).for_each(target); x.ping(); }\npub fn fallback(x: &dyn I, y: &Hub) { (*x).uncommon_s4(); let alias = y; alias.indirect_ping(); }\n";

#[test]
fn s4_initialize_schema_upgrades_pre_s4_refs_shape() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    conn.execute_batch("DROP INDEX IF EXISTS idx_refs_unresolved_last_segment; ALTER TABLE refs DROP COLUMN callee_last_segment;").unwrap();
    initialize_schema(&conn).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM pragma_table_info('refs') WHERE name='callee_last_segment'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='idx_refs_unresolved_last_segment'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("WHERE kind = 'call' AND status = 'unresolved'"));
}

pub(crate) const COLLISIONS: &[(&str, &str, &str)] = &[
    ("swap.rs", "rust", "fn first() {}\nfn other() { first(); }\n"),
    ("overload.ts", "typescript", "function f(x: number): number;\nfunction f(x: string): string;\nfunction f(x: any): any { return x; }\nfunction caller() { f(1); }\n"),
    ("cfg.rs", "rust", "#[cfg(unix)] fn f() {}\n#[cfg(windows)] fn f() {}\nfn caller() { f(); }\n"),
    ("impl.rs", "rust", "struct T;\nimpl T { fn f(&self) {} }\nimpl T { fn f(&self, x: i32) {} }\nfn caller(x: &T) { x.f(); }\n"),
];

#[test]
fn s4_two_scoped_names_share_a_range_without_identity_collision() {
    let source = "fn first() {}\nfn other() {}\n";
    let mut data = callgraph::build_file_data_from_source(Path::new("fixture.rs"), source).unwrap();
    let range = data.symbol_metadata["first"].range.clone();
    data.symbol_metadata.get_mut("other").unwrap().range = range;
    let nodes = build_node_records("fixture.rs", "", &data).unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].range, nodes[1].range);
    assert_eq!((nodes[0].range_ordinal, nodes[1].range_ordinal), (0, 1));
    assert_ne!(nodes[0].id, nodes[1].id);
}

#[test]
fn node_ids_are_short_digests_that_sort_like_the_full_digest() {
    let id = node_id("a.rs", "function", "f");
    let hex = id.strip_prefix("pos:symbol:").unwrap();
    assert_eq!(hex.len(), 16);
    assert!(hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let top = node_id("a.rs", "module", TOP_LEVEL_SYMBOL);
    assert_eq!(top.strip_prefix("top:").unwrap().len(), 16);
    // Ordering by the stored id must match ordering by the full digest it was
    // cut from, since replies order nodes by id.
    let mut ids = (0..200)
        .map(|n| {
            let mut hash = blake3::Hasher::new();
            hash.update(n.to_string().as_bytes());
            let full = hash.finalize();
            (short_hash_hex(full), hash_to_hex(full))
        })
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| left.0.cmp(&right.0));
    assert!(ids.windows(2).all(|pair| pair[0].1 < pair[1].1));
}

#[test]
fn two_symbols_with_one_node_id_fail_the_write_instead_of_merging() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    let insert = |scoped_name: &str| {
        conn.execute(
            "INSERT INTO nodes(id, file_path, name, scoped_name, kind, start_line, start_col,
                 end_line, end_col, range_ordinal, signature, exported, is_default_export,
                 is_type_like, is_callgraph_entry_point, provenance, is_test)
             VALUES('pos:symbol:0123456789abcdef', 'a.rs', ?1, ?1, 'function', 1, 0, 1, 1, 0,
                 NULL, 0, 0, 0, 0, 'treesitter', 0)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, scoped_name=excluded.scoped_name",
            [scoped_name],
        )
    };
    insert("f").unwrap();
    // The same symbol again is an ordinary upsert.
    insert("f").unwrap();
    let error = insert("g").unwrap_err().to_string();
    assert!(error.contains("call-graph node id collision"), "{error}");
    let error = conn
        .execute(
            "INSERT OR REPLACE INTO nodes(id, file_path, name, scoped_name, kind, start_line,
                 start_col, end_line, end_col, range_ordinal, signature, exported,
                 is_default_export, is_type_like, is_callgraph_entry_point, provenance, is_test)
             VALUES('pos:symbol:0123456789abcdef', 'b.rs', 'f', 'f', 'function', 9, 0, 9, 1, 0,
                 NULL, 0, 0, 0, 0, 'treesitter', 0)",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("call-graph node id collision"), "{error}");
    let scoped: String = conn
        .query_row("SELECT scoped_name FROM nodes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(scoped, "f");
}

#[test]
fn s4_node_identity_uses_path_kind_and_scoped_name_only() {
    assert_ne!(
        node_id("a:b", "function", "c"),
        node_id("a", "function", "b:c")
    );
    assert_ne!(
        node_id("a.rs", "function", "f"),
        node_id("a.rs", "method", "f")
    );
    assert_ne!(
        node_id("a.rs", "function", "f"),
        node_id("b.rs", "function", "f")
    );
    let first = callgraph::build_file_data_from_source(Path::new("a.rs"), "fn f() {}\nfn g() {}\n")
        .unwrap();
    let moved =
        callgraph::build_file_data_from_source(Path::new("a.rs"), "fn g() {}\n\nfn f() {}\n")
            .unwrap();
    let ids = |source, data: &FileCallData| {
        build_node_records("a.rs", source, data)
            .unwrap()
            .into_iter()
            .map(|n| (n.scoped_name, n.id))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        ids("fn f() {}\nfn g() {}\n", &first),
        ids("fn g() {}\n\nfn f() {}\n", &moved)
    );
}

thread_local! {
    static PRE_S4_OUTPUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn pre_s4_output() -> bool {
    PRE_S4_OUTPUT.with(|value| value.get())
}

// Exercise the actual row writers with their former identity producers. Every
// reference, edge and dispatch hint is then bound to an old node, not merely
// a current graph with an obsolete version stamp.
pub(crate) fn with_pre_s4_output<T>(run: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            PRE_S4_OUTPUT.with(|v| v.set(self.0));
        }
    }
    let _reset = Reset(PRE_S4_OUTPUT.with(|v| v.replace(true)));
    run()
}

pub(crate) fn stamp_pre_s4(conn: &Connection, views: bool) {
    conn.execute(
        "UPDATE meta SET v=?1 WHERE k='fingerprint'",
        [schema_fingerprint_for("v18-rust-static-value-references")],
    )
    .unwrap();
    if views {
        conn.execute(
            "UPDATE meta SET v='11' WHERE k='view_materialization_version'",
            [],
        )
        .unwrap();
    }
    conn.execute_batch("DROP INDEX idx_refs_unresolved_last_segment; ALTER TABLE refs DROP COLUMN callee_last_segment;").unwrap();
    assert!(
        conn.query_row(
            "SELECT count(*) FROM nodes WHERE id LIKE 'pos:%' OR id LIKE 'view:%'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap()
            > 0
    );
}

pub(crate) fn assert_current_graph(conn: &Connection) {
    assert!(database_ready(conn).unwrap());
    assert!(
        conn.query_row("SELECT count(*) FROM edges", [], |r| r.get::<_, usize>(0))
            .unwrap()
            > 0
    );
    let index: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='idx_refs_unresolved_last_segment'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(index.contains("WHERE kind = 'call' AND status = 'unresolved'"));
    let mut query = conn
        .prepare("SELECT short_name,callee_last_segment FROM refs WHERE short_name IS NOT NULL")
        .unwrap();
    let rows = query
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(!rows.is_empty());
    for (short, last) in rows {
        assert_eq!(last.as_deref(), Some(callee_last_segment(&short)));
    }
    // The views dispatch tables store scoped names, files and ordinals, not
    // node ids. The durable node-id columns are in the graph tables below.
    for (table, column) in [
        ("nodes", "id"),
        ("refs", "caller_node"),
        ("refs", "target_node"),
        ("edges", "source_node"),
        ("edges", "target_node"),
        ("dispatch_hints", "caller_node"),
    ] {
        let old: usize = conn.query_row(&format!("SELECT count(*) FROM {table} WHERE {column} IS NOT NULL AND {column} NOT LIKE 'top:%' AND {column} NOT LIKE 'pos:symbol:%' AND {column} NOT LIKE 'vp%' AND {column} NOT LIKE 'vt%'"), [], |r| r.get(0)).unwrap();
        assert_eq!(old, 0, "old identity in {table}.{column}");
    }
}

#[test]
fn s4_upgrade_views_off_generation_and_legacy_refresh() {
    for legacy in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let file = root.join("fixture.ts");
        fs::write(
            &file,
            "export class C { unique() {} }\nexport function caller(x) { x.unique(); }\n",
        )
        .unwrap();
        let dir = temp.path().join("store");
        let store = CallGraphStore::open(dir.clone(), root.clone()).unwrap();
        with_pre_s4_project(&root, || store.cold_build(std::slice::from_ref(&file))).unwrap();
        let path = store.sqlite_path.clone();
        let key = store.project_key.clone();
        {
            let conn = store.conn.lock().unwrap();
            assert!(
                conn.query_row("SELECT count(*) FROM dispatch_hints", [], |r| r
                    .get::<_, usize>(0))
                    .unwrap()
                    > 0
            );
            stamp_pre_s4(&conn, false);
        }
        drop(store);
        if legacy {
            fs::rename(&path, legacy_sqlite_path(&dir, &key)).unwrap();
            let opened = CallGraphStore::open(dir.clone(), root.clone()).unwrap();
            assert!(!database_ready(&opened.conn.lock().unwrap()).unwrap());
            assert!(opened.refresh_files(std::slice::from_ref(&file)).is_err());
            drop(opened);
        }
        let (store, _) =
            CallGraphStore::ensure_built_with_lease(dir, root, &[file.clone()]).unwrap();
        store.refresh_files(&[file]).unwrap();
        assert_current_graph(&store.conn.lock().unwrap());
        assert!(!store.edge_snapshot().unwrap().is_empty());
    }
}

#[test]
fn s4_upgrade_unversioned_staging_extracting_and_ready_are_removed() {
    for phase in ["extracting", "ready"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let file = root.join("fixture.ts");
        fs::write(
            &file,
            "export class C { unique() {} }\nexport function caller(x) { x.unique(); }\n",
        )
        .unwrap();
        let dir = temp.path().join("store");
        let store = CallGraphStore::open(dir.clone(), root.clone()).unwrap();
        with_pre_s4_project(&root, || store.cold_build(std::slice::from_ref(&file))).unwrap();
        let path = store.sqlite_path.clone();
        let key = store.project_key.clone();
        {
            let conn = store.conn.lock().unwrap();
            stamp_pre_s4(&conn, false);
            conn.execute("UPDATE meta SET v=?1 WHERE k='staged_build_phase'", [phase])
                .unwrap();
        }
        drop(store);
        let unversioned = dir.join(format!("{key}.staging.sqlite.tmp.resume"));
        fs::rename(&path, &unversioned).unwrap();
        assert_ne!(dir.join(staging_file_name(&key)), unversioned);
        let (store, _) = CallGraphStore::cold_build_with_lease(dir, root, &[file]).unwrap();
        assert!(!unversioned.exists());
        assert_current_graph(&store.conn.lock().unwrap());
        assert!(!store.edge_snapshot().unwrap().is_empty());
    }
}

static PRE_S4_PROJECTS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn pre_s4_project(root: &Path) -> bool {
    PRE_S4_PROJECTS.lock().unwrap().iter().any(|p| p == root)
}

pub(crate) fn with_pre_s4_project<T>(root: &Path, run: impl FnOnce() -> T) -> T {
    struct Reset(PathBuf);
    impl Drop for Reset {
        fn drop(&mut self) {
            PRE_S4_PROJECTS.lock().unwrap().retain(|p| p != &self.0);
        }
    }
    PRE_S4_PROJECTS.lock().unwrap().push(root.to_path_buf());
    let _reset = Reset(root.to_path_buf());
    run()
}

pub(crate) fn seed_old_dispatch_hint(conn: &Connection) {
    conn.execute("INSERT INTO dispatch_hints SELECT 'pre-s4-hint', 'unique', id, file_path, start_line, 0, 0, 'treesitter' FROM nodes ORDER BY id LIMIT 1", []).unwrap();
    assert!(conn.query_row("SELECT count(*) FROM dispatch_hints WHERE caller_node NOT LIKE 'pos:symbol:%' AND caller_node NOT LIKE 'vp%' AND caller_node NOT LIKE 'vt%'", [], |r| r.get::<_,usize>(0)).unwrap()>0);
}

#[test]
fn s4_upgrade_old_legacy_single_file_and_superseded_generation_are_not_migrated() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    let file = root.join("fixture.ts");
    fs::write(
        &file,
        "export class C { unique() {} }\nexport function caller(x) { x.unique(); }\n",
    )
    .unwrap();
    let storage = temp.path().join("storage");
    let legacy_dir = storage.join("opencode/callgraph");
    let build_dir = temp.path().join("old-build");
    let store = CallGraphStore::open(build_dir.clone(), root.clone()).unwrap();
    with_pre_s4_project(&root, || store.cold_build(std::slice::from_ref(&file))).unwrap();
    let path = store.sqlite_path.clone();
    let key = store.project_key.clone();
    stamp_pre_s4(&store.conn.lock().unwrap(), false);
    drop(store);
    fs::create_dir_all(&legacy_dir).unwrap();
    let single = legacy_sqlite_path(&legacy_dir, &key);
    fs::copy(&path, &single).unwrap();
    let current = legacy_dir.join(path.file_name().unwrap());
    fs::copy(&path, &current).unwrap();
    fs::write(
        pointer_path(&legacy_dir, &key),
        current.file_name().unwrap().to_string_lossy().as_bytes(),
    )
    .unwrap();
    let superseded = legacy_dir.join(format!("{key}.g1.1.sqlite"));
    fs::copy(&path, &superseded).unwrap();
    let old_bytes = fs::read(&superseded).unwrap();
    let target = storage.join("callgraph").join(&key);
    crate::root_cache::configure_artifact_access(&root, &key, false);
    assert!(
        CallGraphStore::migrate_legacy_with_lease(target.clone(), root.clone())
            .unwrap()
            .is_none()
    );
    let (store, stats) = CallGraphStore::ensure_built_with_lease(target, root, &[file]).unwrap();
    assert!(
        stats.is_some(),
        "old migration sources must force a cold build"
    );
    assert_current_graph(&store.conn.lock().unwrap());
    assert_eq!(fs::read(&superseded).unwrap(), old_bytes);
    assert_eq!(fs::read(&single).unwrap(), old_bytes);
}

pub(crate) fn language_fixtures() -> [(&'static str, &'static str, &'static str); 33] {
    [
        (
            "a.ts",
            "typescript",
            "function f() {}\nfunction g() { f(); }",
        ),
        ("a.tsx", "tsx", "function F() { return <div/>; }"),
        (
            "a.js",
            "javascript",
            "function f() {}\nfunction g() { f(); }",
        ),
        ("a.py", "python", "def f(): pass\ndef g(): f()\n"),
        ("a.rs", "rust", "fn f() {}\nfn g() { f(); }"),
        ("a.go", "go", "package p\nfunc f() {}\nfunc g() { f() }"),
        ("a.c", "c", "void f() {}\nvoid g() { f(); }"),
        ("a.cpp", "cpp", "void f() {}\nvoid g() { f(); }"),
        ("a.cu", "cuda", "__global__ void f() {}"),
        ("a.metal", "metal", "void f() {}"),
        ("a.zig", "zig", "fn f() void {}"),
        (
            "a.cs",
            "csharp",
            "class C { void F() {} void G() { F(); } }",
        ),
        ("a.sh", "bash", "f() { echo hello; }"),
        ("a.sol", "solidity", "contract C { function f() public {} }"),
        ("a.scss", "scss", "@mixin f { color: red; }"),
        (
            "a.vue",
            "vue",
            "<script>function f() {}</script><template><div/></template>",
        ),
        ("a.json", "json", "{\"f\": 1, \"g\": {\"h\": 2}}"),
        ("a.scala", "scala", "object C { def f(): Unit = {} }"),
        (
            "a.java",
            "java",
            "class C { void f() {} void g() { f(); } }",
        ),
        ("a.rb", "ruby", "def f\nend\ndef g\n f\nend\n"),
        ("a.kt", "kotlin", "fun f() {}\nfun g() { f() }"),
        ("a.swift", "swift", "func f() {}\nfunc g() { f() }"),
        (
            "a.php",
            "php",
            "<?php function f() {} function g() { f(); }",
        ),
        ("a.lua", "lua", "function f() end\nfunction g() f() end"),
        ("a.pl", "perl", "sub f {}\nsub g { f(); }"),
        ("a.html", "html", "<h1>Main Title</h1>"),
        ("a.md", "markdown", "# Heading\n\n## Child\n"),
        ("a.yaml", "yaml", "f: 1\ng:\n  h: 2\n"),
        (
            "a.pas",
            "pascal",
            "program P; procedure F; begin end; begin F; end.",
        ),
        ("a.r", "r", "f <- function() {}\ng <- function() { f() }"),
        ("a.groovy", "groovy", "class C { def f() {} }"),
        (
            "a.m",
            "objc",
            "@interface C\n- (void)f;\n@end\n@implementation C\n- (void)f {}\n@end",
        ),
        ("a.toml", "toml", "[f]\ng = 1\n"),
    ]
}

#[test]
fn s4_one_node_per_scoped_name_for_every_supported_language() {
    for (path, language, source) in language_fixtures() {
        let data = callgraph::build_file_data_from_source(Path::new(path), source).unwrap();
        let nodes = build_node_records(path, source, &data).unwrap();
        assert!(!nodes.is_empty(), "{language} fixture must define a symbol");
        assert_eq!(nodes.len(), data.symbol_metadata.len(), "{language}");
        assert_eq!(
            nodes
                .iter()
                .map(|n| &n.scoped_name)
                .collect::<BTreeSet<_>>()
                .len(),
            nodes.len(),
            "{language}"
        );
        let blob =
            join::CallgraphBlob::extract(source, language, crate::callgraph_extractor::PRODUCER)
                .unwrap();
        let symbols = &blob.parse().unwrap().symbols;
        assert!(!symbols.is_empty(), "{language}");
        assert_eq!(
            symbols
                .iter()
                .map(|s| &s.scoped_name)
                .collect::<BTreeSet<_>>()
                .len(),
            symbols.len(),
            "{language}"
        );
        assert_eq!(
            symbols
                .iter()
                .map(|s| view_node_id(path, &s.kind, &s.scoped_name))
                .collect::<BTreeSet<_>>()
                .len(),
            symbols.len(),
            "{language}"
        );
    }
}

static SHARED_RANGE_PROJECTS: std::sync::Mutex<BTreeSet<PathBuf>> =
    std::sync::Mutex::new(BTreeSet::new());

pub(crate) fn shared_range_fixture(root: &Path, data: &mut FileCallData) -> bool {
    if !SHARED_RANGE_PROJECTS.lock().unwrap().contains(root) {
        return false;
    }
    let first = data.symbol_metadata["first"].range.clone();
    data.symbol_metadata.get_mut("other").unwrap().range = first;
    true
}

