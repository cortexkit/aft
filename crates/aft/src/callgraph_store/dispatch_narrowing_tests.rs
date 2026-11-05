//! Method-dispatch narrowing in `CallGraphStore` refreshes (the store used
//! when index views are disabled): which callers a refresh recomputes
//! dispatch edges for. Each edit is checked by comparing every table of the
//! refreshed store with a cold build of the edited tree.

use super::*;
use std::fs;

/// The hub file `hub.rs`. Its methods receive name-match and type-match edges from
/// three caller files, path-form `Hub::build()` calls from two more files, and
/// it holds a dispatch call of its own.
const HUB_RS: &str = "pub struct Hub;
pub struct Other;
impl Hub {
    pub fn ping(&self) {}
    pub fn pong(&self) {}
    pub fn moved(&self) {}
    pub fn renamed(&self) {}
    pub fn build() -> Hub { Hub }
}
impl Other {
    pub fn other_only(&self) {}
}
pub fn helper() {}
pub fn own_call(x: &Hub) { x.ping(); }
";

/// `x.*` calls get type-match edges (the receiver's type is in the signature);
/// `alias.*` calls get name-match edges.
const CALLER_RS: &str = "use crate::hub::{self, Hub};
pub fn typed(x: &Hub) { x.ping(); x.moved(); x.renamed(); }
pub fn named(y: &Hub) { let alias = y; alias.pong(); alias.moved(); alias.renamed(); }
pub fn direct() { hub::helper(); }
";

const PATH_CALLER_RS: &str = "use crate::hub::Hub;
pub fn path_calls() { Hub::build(); Hub::not_yet(); }
";

const EXTRA_RS: &str = "pub struct Extra;
impl Extra {
    pub fn unrelated(&self) {}
}
";

const LIB_RS: &str = "mod hub; mod caller0; mod caller1; mod caller2; mod path0; mod path1; mod extra; mod ghost_matching; mod ghost_unrelated;\n";

/// `ns.helper()` resolves directly while `web/hub.ts` exports `helper`, and
/// falls back to a name-match edge into `web/tsother.ts` when it does not.
const HUB_TS: &str = "export class Box { open(): void {} }\nexport function helper(): void {}\n";
const TS_CALLER: &str = "import * as ns from './hub';\nexport function useIt(b: any): void { ns.helper(); b.open(); }\n";
const TS_OTHER: &str = "export class Crate { helper(): void {} }\n";

/// Writes the fixture: `hub.rs`, its callers, and unresolved path-form calls whose
/// last segment is `probe` (`matching` of them) or `elsewhere` (`unrelated`).
fn write_fixture(root: &Path, matching: usize, unrelated: usize) {
    fs::create_dir_all(root).unwrap();
    let put = |rel: &str, text: &str| fs::write(root.join(rel), text).unwrap();
    put("hub.rs", HUB_RS);
    for n in 0..3 {
        put(&format!("caller{n}.rs"), CALLER_RS);
    }
    for n in 0..2 {
        put(&format!("path{n}.rs"), PATH_CALLER_RS);
    }
    put("extra.rs", EXTRA_RS);
    let calls = |call: &str, count: usize| {
        let mut text = String::from("pub fn calls() {\n");
        for _ in 0..count {
            text.push_str(&format!("    {call}();\n"));
        }
        text.push_str("}\n");
        text
    };
    put("ghost_matching.rs", &calls("Ghost::probe", matching));
    put(
        "ghost_unrelated.rs",
        &calls("Phantom::elsewhere", unrelated),
    );
    put("lib.rs", LIB_RS);
    put(
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n",
    );
    // A subdirectory keeps `./hub` from also naming the Rust `hub.rs`.
    fs::create_dir_all(root.join("web")).unwrap();
    put("web/hub.ts", HUB_TS);
    put("web/tscaller.ts", TS_CALLER);
    put("web/tsother.ts", TS_OTHER);
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in [root.to_path_buf(), root.join("web")] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        files.extend(entries.map(|entry| entry.unwrap().path()).filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "rs" | "ts" | "py" | "java" | "cpp" | "go"))
        }));
    }
    files.sort();
    files
}

fn edit(root: &Path, rel: &str, from: &str, to: &str) -> PathBuf {
    let path = root.join(rel);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(from), "{rel} does not contain {from:?}");
    fs::write(&path, text.replacen(from, to, 1)).unwrap();
    path
}

fn count(store: &CallGraphStore, sql: &str) -> usize {
    store
        .conn
        .lock()
        .unwrap()
        .query_row(sql, [], |row| row.get::<_, usize>(0))
        .unwrap()
}

fn set(files: &[&str]) -> BTreeSet<String> {
    files.iter().map(|file| file.to_string()).collect()
}

fn go_typed_edges(store: &CallGraphStore) -> Vec<(String, String, String)> {
    let conn = store.conn.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT r.caller_file, e.target_file, e.target_symbol FROM edges e
         JOIN refs r ON r.ref_id=e.ref_id JOIN files f ON f.path=r.caller_file
         WHERE f.lang='go' AND e.provenance='type_match' ORDER BY 1,2,3",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn go_method_answers(conn: &Connection) -> Vec<(String, String, String, String, u32)> {
    let mut stmt = conn
        .prepare(
            "SELECT r.caller_file, src.scoped_name, e.target_file, e.target_symbol, r.line
        FROM edges e JOIN refs r ON r.ref_id=e.ref_id JOIN nodes src ON src.id=e.source_node
        WHERE e.kind='call' AND e.target_symbol LIKE '%::%' ORDER BY 1,2,3,4,5",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<std::result::Result<Vec<_>, _>>()
    .unwrap()
}

fn go_view_answers(
    fixture: &Fixture,
    package_known: bool,
    unknown_file: Option<&str>,
) -> Vec<(String, String, String, String, u32)> {
    use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};
    let temp = tempfile::tempdir_in(fixture.temp.path()).unwrap();
    let blobs_path = temp.path().join("blobs.sqlite");
    let blobs = Connection::open(&blobs_path).unwrap();
    blobs.execute_batch("CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB NOT NULL, payload_digest BLOB NOT NULL, payload_schema INTEGER NOT NULL)").unwrap();
    let mut entries = Vec::new();
    for path in source_files(&fixture.root) {
        let source = fs::read_to_string(&path).unwrap();
        let blob =
            join::CallgraphBlob::extract(&source, "go", crate::callgraph_extractor::PRODUCER)
                .unwrap();
        let mut value = serde_json::to_value(&blob).unwrap();
        if !package_known || unknown_file == Some(relative_path(&fixture.root, &path).as_str()) {
            value["dispatch"]
                .as_object_mut()
                .unwrap()
                .remove("go_package");
        }
        let blob: join::CallgraphBlob = serde_json::from_value(value).unwrap();
        let payload = blob.to_bytes().unwrap();
        let key = blake3::hash(&payload);
        blobs
            .execute(
                "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
                params![
                    key.as_bytes().as_slice(),
                    payload,
                    key.as_bytes().as_slice()
                ],
            )
            .unwrap();
        entries.push((
            RelPath::new(relative_path(&fixture.root, &path).into_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    semantic: None,
                    callgraph: Some(key.to_hex().to_string()),
                },
                resolution_input: false,
            },
        ));
    }
    let manifest = Manifest::new(entries).unwrap();
    let database = temp.path().join("view.sqlite");
    crate::views::materialization::materialize_manifest_view_database(
        &database,
        &blobs_path,
        &manifest,
    )
    .unwrap();
    go_method_answers(&Connection::open(database).unwrap())
}

fn assert_go_concrete_view_parity(fixture: &Fixture, has_interface: bool) {
    let legacy = go_method_answers(&fixture.store.conn.lock().unwrap());
    let mut viewed = go_view_answers(fixture, true, None);
    // A known interface receiver deliberately retains declaration/implementation
    // fan-out in views; the retiring store only resolves concrete receivers.
    let interface = viewed
        .iter()
        .filter(|e| e.1 == "contract")
        .map(|e| e.3.as_str())
        .collect::<BTreeSet<_>>();
    if has_interface {
        assert_eq!(interface, BTreeSet::from(["I::run", "T::run"]));
    } else {
        assert!(interface.is_empty());
    }
    viewed.retain(|e| e.1 != "contract");
    assert_eq!(legacy, viewed);
}

#[test]
fn go_concrete_method_receivers_bind_across_test_package_files() {
    let fixture = Fixture::from_files(&[
        ("suite_test.go", "package db\ntype testSuite struct{}\n"),
        ("issue_test.go", "package db\nfunc (s *testSuite) issueUseCase() {}\n"),
        ("close_test.go", "package db\nfunc (s *testSuite) close() { s.issueUseCase() }\nfunc (s testSuite) check() { s.issueUseCase() }\n"),
        ("external_test.go", "package db_test\ntype testSuite struct{}\nfunc (s *testSuite) issueUseCase() {}\n"),
        ("web/other.go", "package db\ntype testSuite struct{}\nfunc (s *testSuite) issueUseCase() {}\n"),
    ]);
    assert_eq!(
        go_typed_edges(&fixture.store),
        vec![
            (
                "close_test.go".into(),
                "issue_test.go".into(),
                "testSuite::issueUseCase".into()
            ),
            (
                "close_test.go".into(),
                "issue_test.go".into(),
                "testSuite::issueUseCase".into()
            ),
        ]
    );
    assert_go_concrete_view_parity(&fixture, false);
}

#[test]
fn go_concrete_factory_return_and_local_declarations_bind_exactly() {
    let fixture = Fixture::from_files(&[
        ("fixture_test.go", "package p\ntype Fixture struct{}\nfunc (f *Fixture) commitLane() {}\ntype Other struct{}\nfunc (f *Other) commitLane() {}\ntype Contract interface { commitLane() }\n"),
        ("caller_test.go", "package p\nfunc newFixture() *Fixture { return &Fixture{} }\nfunc caller() {\n f := newFixture(); f.commitLane()\n var x Fixture; x.commitLane()\n y := &Fixture{}; y.commitLane()\n z := Fixture{}; z.commitLane()\n}\n"),
    ]);
    assert_eq!(
        go_typed_edges(&fixture.store),
        vec![
            (
                "caller_test.go".into(),
                "fixture_test.go".into(),
                "Fixture::commitLane".into()
            );
            4
        ]
    );
    assert_go_concrete_view_parity(&fixture, false);
    let changed = edit(
        &fixture.root,
        "caller_test.go",
        "newFixture() *Fixture { return &Fixture{} }",
        "newFixture() *Other { return &Other{} }",
    );
    fixture.refresh_and_compare("go-factory-result", &[changed]);
    assert_eq!(
        go_typed_edges(&fixture.store),
        vec![
            (
                "caller_test.go".into(),
                "fixture_test.go".into(),
                "Fixture::commitLane".into()
            ),
            (
                "caller_test.go".into(),
                "fixture_test.go".into(),
                "Fixture::commitLane".into()
            ),
            (
                "caller_test.go".into(),
                "fixture_test.go".into(),
                "Fixture::commitLane".into()
            ),
            (
                "caller_test.go".into(),
                "fixture_test.go".into(),
                "Other::commitLane".into()
            ),
        ]
    );
    assert_go_concrete_view_parity(&fixture, false);
}

#[test]
fn go_concrete_binding_refuses_shadows_interfaces_and_nonlocal_packages() {
    let fixture = Fixture::from_files(&[
        ("types.go", "package p\ntype T struct{}\nfunc (x *T) run() {}\ntype I interface { run() }\nfunc factory() *T { return &T{} }\n"),
        ("caller_test.go", "package p\nfunc shadowed(x *T) { { x := unknown(); x.run() }; x.run() }\nfunc localType() { type T struct{}; var x T; x.run() }\nfunc shadowFactory(factory func() I) { x := factory(); x.run() }\nfunc contract(x I) { x.run() }\nfunc sibling() { { var x T }; x.run() }\nfunc closure(x *T) { f := func(x I) { x.run() }; _ = f }\nfunc switched(x *T, value any) { switch x := value.(type) { case I: x.run() } }\nfunc variadic(x ...T) { x.run() }\nfunc pointer(x **T) { x.run() }\nfunc unknown() I { return nil }\n"),
        ("external_test.go", "package p_test\nfunc outsider(x *T) { x.run() }\n"),
    ]);
    assert!(go_typed_edges(&fixture.store).is_empty());
    assert_go_concrete_view_parity(&fixture, true);
}

#[test]
fn go_concrete_missing_package_fact_is_unknown_in_views() {
    let fixture = Fixture::from_files(&[
        (
            "types.go",
            "package p\ntype T struct{}\nfunc (x *T) run() {}\n",
        ),
        ("caller.go", "package p\nfunc (x *T) caller() { x.run() }\n"),
    ]);
    assert_go_concrete_view_parity(&fixture, false);
    assert_eq!(go_typed_edges(&fixture.store).len(), 1);
    assert!(go_view_answers(&fixture, false, None).is_empty());
    let mixed = Fixture::from_files(&[
        ("interface.go", "package p\ntype I interface { run() }\n"),
        (
            "types.go",
            "package p\ntype T struct{}\nfunc (x *T) run() {}\n",
        ),
        ("caller.go", "package p\nfunc contract(x I) { x.run() }\n"),
    ]);
    let viewed = go_view_answers(&mixed, true, Some("types.go"));
    assert_eq!(viewed.len(), 1);
    assert_eq!(
        viewed[0].3, "I::run",
        "an old implementation fact cannot acquire a binding without package identity"
    );
}

#[test]
fn go_concrete_repeated_callback_parameters_are_scoped() {
    let fixture = Fixture::from_files(&[
        ("types.go", "package p\ntype T struct{}\nfunc (x *T) run() {}\ntype Other struct{}\nfunc (x *Other) run() {}\nvar x *Other\n"),
        ("caller.go", "package p\nfunc caller() {\n callbacks := []func(x *T){\n  func(x *T) { x.run() },\n  func(x *T) { x.run() },\n }\n _ = callbacks\n x.run()\n other := func() { var x *Other; x.run() }; _ = other\n}\n"),
    ]);
    assert_eq!(
        go_typed_edges(&fixture.store),
        vec![("caller.go".into(), "types.go".into(), "T::run".into()); 2]
    );
    let answers = go_method_answers(&fixture.store.conn.lock().unwrap());
    assert_eq!(answers.iter().map(|e| e.4).collect::<Vec<_>>(), vec![4, 5]);
    assert_go_concrete_view_parity(&fixture, false);
    for source in [
        "package p\nfunc caller() { callbacks := []any{func(x *T) { x.run() }, func(x *Other) { x.run() }}; _=callbacks }\n",
        "package p\nfunc unknown() interface { run() } { return nil }\nfunc caller() { callbacks := []any{func(x *T) { { x := unknown(); x.run() } }, func(x *T) { x.run() }}; _=callbacks }\n",
        "package p\nimport . \"net/http\"\nfunc caller() { _=StatusOK; callbacks := []any{func(x *T) { x.run() }, func(x *T) { x.run() }}; _=callbacks }\n",
    ] {
        let refused = Fixture::from_files(&[
            ("types.go", "package p\ntype T struct{}\nfunc (x *T) run() {}\ntype Other struct{}\nfunc (x *Other) run() {}\n"),
            ("caller.go", source),
        ]);
        assert!(go_typed_edges(&refused.store).is_empty());
        assert_go_concrete_view_parity(&refused, false);
    }
}

/// A cold-built fixture store that edit shapes are applied to.
struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    store: CallGraphStore,
}

#[test]
fn typescript_object_store_records_wrapped_returned_and_argument_methods() {
    let fixture = Fixture::from_files(&[
        ("producer.ts", "export const make = () => ({ run: () => {} });\nregister({ argument: () => {} });"),
        ("caller.ts", "import { make as build } from './producer';\nexport function caller() { const adapter = build(); adapter.run(); }\nconst direct = { wrapped: () => {} } satisfies Contract;\ndirect.wrapped();\nfunction unknown(value) { value.argument(); }\nargument();"),
    ]);
    assert_eq!(count(&fixture.store, "SELECT COUNT(*) FROM nodes WHERE name IN ('run','argument','wrapped') AND kind='method'"), 3);
    assert_eq!(count(&fixture.store, "SELECT COUNT(*) FROM edges WHERE target_symbol LIKE '@object:%' AND provenance='type_match'"), 2);
    assert_eq!(
        count(
            &fixture.store,
            "SELECT COUNT(*) FROM edges WHERE target_symbol LIKE '%::argument'"
        ),
        0
    );
}

#[test]
fn typescript_object_store_factory_edits_match_a_cold_build() {
    let fixture = Fixture::from_files(&[
        ("producer.ts", "export function make() { return { run() {} }; }"),
        ("caller.ts", "import { make } from './producer';\nexport function caller() { const adapter = make(); adapter.run(); }"),
    ]);
    assert_eq!(count(&fixture.store, "SELECT COUNT(*) FROM edges WHERE target_symbol LIKE '@object:%' AND provenance='type_match'"), 1);
    let changed = edit(
        &fixture.root,
        "producer.ts",
        "return { run() {} };",
        "const unused = { run() {} }; return unknown;",
    );
    fixture.refresh_and_compare("typescript-object-return-removed", &[changed]);
    assert_eq!(
        count(
            &fixture.store,
            "SELECT COUNT(*) FROM edges WHERE target_symbol LIKE '@object:%'"
        ),
        0
    );
}

#[test]
fn typescript_object_store_var_definitions_are_not_bare_name_bindings() {
    let fixture = Fixture::from_files(&[
        ("caller.ts", "var value = { unique() {} };\nfunction caller(unknown) { unknown.unique(); unique(); value.unique(); }"),
    ]);
    assert_eq!(
        count(
            &fixture.store,
            "SELECT COUNT(*) FROM nodes WHERE name='unique' AND kind='method'"
        ),
        1
    );
    assert_eq!(
        count(
            &fixture.store,
            "SELECT COUNT(*) FROM edges WHERE target_symbol LIKE '%::unique'"
        ),
        1
    );
    assert_eq!(count(&fixture.store, "SELECT COUNT(*) FROM edges WHERE target_symbol LIKE '%::unique' AND provenance='type_match'"), 1);
}

#[test]
fn typescript_object_cold_cache_avoids_reextraction() {
    let fixture = Fixture::from_files(&[
        ("producer.ts", "export const make = () => ({ run() {} });"),
        ("caller.ts", "import {make} from './producer'; function caller() { const adapter=make(); adapter.run(); }"),
    ]);
    let mut retained = RetainedCallerData::new(1024 * 1024);
    for path in source_files(&fixture.root) {
        let extract = build_file_extract(&fixture.root, &path).unwrap();
        retained.retain_objects(&extract);
    }
    let connection = fixture.store.conn.lock().unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    let references = load_name_match_refs(&transaction, None).unwrap();
    assert!(references
        .iter()
        .any(|reference| reference.method_name == "run"));
    let mut objects = typescript_objects::ObjectDispatch::new(
        &transaction,
        &fixture.root,
        Some(&retained.objects),
    );
    objects.prepare(&references).unwrap();
    assert_eq!(objects.source_extracts(), 0);
    assert!(references
        .iter()
        .any(|reference| objects.resolve(&transaction, reference).unwrap().is_some()));
}

#[test]
fn typescript_object_views_and_legacy_answers_are_identical() {
    use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};
    fn answers(connection: &Connection) -> Vec<(String, String, String, String, u32, bool)> {
        connection.prepare("SELECT src.file_path,src.scoped_name,e.target_file,e.target_symbol,e.line,e.provenance != 'name_match' FROM edges e JOIN nodes src ON src.id=e.source_node WHERE e.kind='call' ORDER BY 1,2,3,4,5,6")
            .unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }
    let cases: &[(&[(&str, &str)], usize)] = &[
        (&[("caller.ts", "interface T { handler(): void }\nconst actual = ({ handler: () => {} } satisfies T);\nconst other = { handler: () => {} } as T;\nactual.handler();\nother.handler();")], 2),
        (&[
            ("producer.ts", "export const make = () => ({ run: () => {} });\nregister({ argument: () => helper() });\nfunction helper() {}"),
            ("caller.ts", "import { make as build } from './producer';\nfunction caller() { const adapter = build(); adapter.run(); }\nfunction nestedCaller() { const nested = () => ({ child: { run() {} } }); nested().child.run(); }\nfunction unknown(value) { value.argument(); }\nargument();"),
        ], 5),
        (&[("caller.ts", "register({ run: () => helper() });\nregister({ run() { helper(); } });\nfunction helper() {}\nfunction first() { const value = { unique() {} }; value.unique(); }\nfunction second() { const value = { unique() {} }; value.unique(); }\nfunction inline() { ({run: function() {}}).run(); }")], 5),
        (&[("caller.ts", "var value = { unique() {} };\nfunction caller(unknown) { unknown.unique(); unique(); value.unique(); }")], 1),
    ];
    for &(files, expected) in cases {
        let fixture = Fixture::from_files(files);
        let blob_path = fixture.temp.path().join("blobs.sqlite");
        let blobs = Connection::open(&blob_path).unwrap();
        blobs.execute_batch("CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY,payload BLOB NOT NULL,payload_digest BLOB NOT NULL,payload_schema INTEGER NOT NULL)").unwrap();
        let manifest = Manifest::new(files.iter().map(|(path, source)| {
            let payload =
                join::CallgraphBlob::extract(source, "typescript", "typescript-object-parity")
                    .unwrap()
                    .to_bytes()
                    .unwrap();
            let digest = blake3::hash(&payload);
            let key = digest.to_hex().to_string();
            blobs
                .execute(
                    "INSERT OR IGNORE INTO blob_payloads VALUES (?1,?2,?3,1)",
                    params![
                        digest.as_bytes().as_slice(),
                        payload,
                        digest.as_bytes().as_slice()
                    ],
                )
                .unwrap();
            (
                RelPath::new(path.as_bytes()).unwrap(),
                ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: RegularPlanes {
                        callgraph: Some(key),
                        semantic: None,
                    },
                    resolution_input: false,
                },
            )
        }))
        .unwrap();
        drop(blobs);
        let view_path = fixture.temp.path().join("view.sqlite");
        crate::views::materialization::materialize_manifest_view_database(
            &view_path, &blob_path, &manifest,
        )
        .unwrap();
        let view = Connection::open(view_path).unwrap();
        let legacy = fixture.store.conn.lock().unwrap();
        let legacy_answers = answers(&legacy);
        assert_eq!(
            legacy_answers.len(),
            expected,
            "{files:?}: {legacy_answers:?}"
        );
        assert_eq!(answers(&view), legacy_answers, "{files:?}");
    }
}

impl Fixture {
    fn new(matching: usize, unrelated: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        write_fixture(&root, matching, unrelated);
        let store = CallGraphStore::open(temp.path().join("incremental"), root.clone()).unwrap();
        store.cold_build(&source_files(&root)).unwrap();
        Self { temp, root, store }
    }

    /// A fixture holding only `files`, written under the project root.
    fn from_files(files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        for (rel, text) in files {
            fs::create_dir_all(root.join(rel).parent().unwrap()).unwrap();
            fs::write(root.join(rel), text).unwrap();
        }
        let store = CallGraphStore::open(temp.path().join("incremental"), root.clone()).unwrap();
        store.cold_build(&source_files(&root)).unwrap();
        Self { temp, root, store }
    }

    /// Refreshes `changed` incrementally, then checks the refresh's revision
    /// and delta and compares every table with a cold build of the same tree.
    fn refresh_and_compare(&self, label: &str, changed: &[PathBuf]) -> IncrementalStats {
        let before = self.store.projection_write_revision().unwrap().unwrap();
        let stats = self.store.refresh_files(changed).unwrap();
        eprintln!(
            "{label}: dispatch_callers_recomputed={:?} dispatch_path_form_rows_examined={}",
            stats.dispatch_callers_recomputed, stats.dispatch_path_form_rows_examined
        );
        let cold_dir = self.temp.path().join(format!("cold-{label}"));
        let cold = CallGraphStore::open(cold_dir, self.root.clone()).unwrap();
        cold.cold_build(&source_files(&self.root)).unwrap();
        incremental_parity::validate_delta(&self.store, before, &stats)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        incremental_parity::compare(&self.store, &cold)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        stats
    }
}

#[test]
fn fixture_has_the_dispatch_shape_the_edit_shapes_rely_on() {
    let fixture = Fixture::new(10, 1000);
    let store = &fixture.store;
    let dispatch_callers = |provenance: &str| {
        count(
            store,
            &format!(
                "SELECT count(DISTINCT r.caller_file) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
                 WHERE e.target_file = 'hub.rs' AND e.provenance = '{provenance}' AND r.caller_file <> 'hub.rs'"
            ),
        )
    };
    assert!(dispatch_callers(PROVENANCE_NAME_MATCH) >= 3);
    assert!(dispatch_callers(PROVENANCE_TYPE_MATCH) >= 3);
    // Unresolved `Hub::method` calls in two files besides the callers of `hub.rs`.
    assert_eq!(
        count(
            store,
            "SELECT count(DISTINCT caller_file) FROM refs
             WHERE kind = 'call' AND status = 'unresolved' AND short_name GLOB 'Hub::*'"
        ),
        2
    );
    // `hub.rs`'s own dispatch call.
    assert_eq!(
        count(
            store,
            "SELECT count(*) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE r.caller_file = 'hub.rs' AND e.provenance = 'type_match'"
        ),
        1
    );
    for (segment, expected) in [("probe", 10), ("elsewhere", 1000)] {
        assert_eq!(
            count(
                store,
                &format!(
                    "SELECT count(*) FROM refs WHERE kind = 'call' AND status = 'unresolved'
                     AND callee_last_segment = '{segment}' AND short_name <> '{segment}'"
                )
            ),
            expected
        );
    }
    // The transitions edit relies on `ns.helper()` resolving directly.
    assert_eq!(
        count(
            store,
            "SELECT count(*) FROM refs WHERE caller_file = 'web/tscaller.ts' AND short_name = 'helper' AND status = 'resolved'"
        ),
        1
    );
}

#[test]
fn s3_a_pure_line_shift_recomputes_only_the_edited_file() {
    let fixture = Fixture::new(10, 1000);
    let path = fixture.root.join("hub.rs");
    fs::write(&path, format!("// shifted\n{HUB_RS}")).unwrap();
    let stats = fixture.refresh_and_compare("a-shift", &[path]);
    assert_eq!(stats.dispatch_callers_recomputed, set(&["hub.rs"]));
    assert_eq!(stats.dispatch_path_form_rows_examined, 0);
}

#[test]
fn s3_b_added_method_examines_only_matching_path_form_rows() {
    for (matching, unrelated) in [(10, 1000), (1000, 10)] {
        let fixture = Fixture::new(matching, unrelated);
        let path = edit(
            &fixture.root,
            "hub.rs",
            "impl Hub {\n",
            "impl Hub {\n    pub fn probe(&self) {}\n",
        );
        let stats = fixture.refresh_and_compare(&format!("b-add-{matching}"), &[path]);
        assert_eq!(
            stats.dispatch_path_form_rows_examined, matching,
            "matching={matching} unrelated={unrelated}"
        );
        assert!(stats
            .dispatch_callers_recomputed
            .contains("ghost_matching.rs"));
        assert!(!stats
            .dispatch_callers_recomputed
            .contains("ghost_unrelated.rs"));
    }
}

#[test]
fn s3_c_removed_method_with_inbound_edges() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(&fixture.root, "hub.rs", "    pub fn pong(&self) {}\n", "");
    let stats = fixture.refresh_and_compare("c-remove", &[path]);
    for caller in ["caller0.rs", "caller1.rs", "caller2.rs"] {
        assert!(stats.dispatch_callers_recomputed.contains(caller));
    }
}

#[test]
fn s3_d_renamed_method() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(&fixture.root, "hub.rs", "fn renamed(", "fn renamed_again(");
    fixture.refresh_and_compare("d-rename", &[path]);
}

#[test]
fn s3_e_method_moved_between_two_types_in_the_file() {
    let fixture = Fixture::new(10, 1000);
    edit(&fixture.root, "hub.rs", "    pub fn moved(&self) {}\n", "");
    let path = edit(
        &fixture.root,
        "hub.rs",
        "impl Other {\n",
        "impl Other {\n    pub fn moved(&self) {}\n",
    );
    fixture.refresh_and_compare("e-move", &[path]);
}

#[test]
fn s3_f_deleted_file() {
    let fixture = Fixture::new(10, 1000);
    // Deleting `hub.rs` also removes its `mod hub;` declaration from
    // `lib.rs`, as deleting a module file normally does.
    let path = fixture.root.join("hub.rs");
    fs::remove_file(&path).unwrap();
    let lib = edit(&fixture.root, "lib.rs", "mod hub; ", "");
    let stats = fixture.refresh_and_compare("f-delete", &[path, lib]);
    for caller in [
        "caller0.rs",
        "caller1.rs",
        "caller2.rs",
        "path0.rs",
        "path1.rs",
    ] {
        assert!(stats.dispatch_callers_recomputed.contains(caller));
    }
}

#[test]
fn s3_g_same_named_method_added_in_another_file() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(
        &fixture.root,
        "extra.rs",
        "impl Extra {\n",
        "impl Extra {\n    pub fn pong(&self) {}\n",
    );
    fixture.refresh_and_compare("g-same-name", &[path]);
}

#[test]
fn s3_h_callsite_only_edit_in_a_caller() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(&fixture.root, "caller0.rs", "alias.pong()", "alias.ping()");
    fixture.refresh_and_compare("h-callsite", &[path]);
}

#[test]
fn s3_i_direct_to_dispatch_and_back() {
    let fixture = Fixture::new(10, 1000);
    // `ns.helper()` in the unedited `web/tscaller.ts` loses its direct target
    // and becomes a name-match call into `web/tsother.ts`...
    let path = edit(
        &fixture.root,
        "web/hub.ts",
        "export function helper(): void {}\n",
        "",
    );
    let stats = fixture.refresh_and_compare("i-direct-to-dispatch", std::slice::from_ref(&path));
    assert!(stats
        .dispatch_callers_recomputed
        .contains("web/tscaller.ts"));
    assert_eq!(
        count(
            &fixture.store,
            "SELECT count(*) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE r.caller_file = 'web/tscaller.ts' AND e.target_file = 'web/tsother.ts' AND e.provenance = 'name_match'"
        ),
        1
    );
    // ...and resolves directly again once the export returns.
    fs::write(&path, HUB_TS).unwrap();
    let stats = fixture.refresh_and_compare("i-dispatch-to-direct", &[path]);
    assert!(stats
        .dispatch_callers_recomputed
        .contains("web/tscaller.ts"));
    assert_eq!(
        count(
            &fixture.store,
            "SELECT count(*) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE r.caller_file = 'web/tscaller.ts' AND e.provenance = 'name_match' AND e.target_file = 'web/tsother.ts'"
        ),
        0
    );
}

/// The plan rows SQLite chooses for `sql`, the path-form dispatch lookup or a
/// variant of it, or the error preparing it.
fn path_form_plan(conn: &Connection, sql: &str) -> std::result::Result<Vec<String>, String> {
    let mut statement = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .map_err(|error| error.to_string())?;
    let plan = statement
        .query_map(params!["probe"], |row| row.get::<_, String>(3))
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    Ok(plan)
}

/// Ok when the lookup searches the partial index and never scans `refs`.
fn check_path_form_plan(
    plan: std::result::Result<Vec<String>, String>,
) -> std::result::Result<(), String> {
    let plan = plan?;
    let uses_index = plan.iter().any(|detail| {
        detail.starts_with("SEARCH refs USING INDEX idx_refs_unresolved_last_segment")
    });
    let scans_refs = plan.iter().any(|detail| detail.starts_with("SCAN refs"));
    if uses_index && !scans_refs {
        Ok(())
    } else {
        Err(format!("path-form lookup plan: {plan:?}"))
    }
}

#[test]
fn s3_path_form_lookup_searches_the_partial_index() {
    let fixture = Fixture::new(10, 1000);
    let conn = fixture.store.conn.lock().unwrap();
    let sql = PATH_FORM_DISPATCH_CALLERS_SQL;
    check_path_form_plan(path_form_plan(&conn, sql)).unwrap();
    // Statistics from `ANALYZE` (which `PRAGMA optimize` can run) must not
    // move the lookup off the index.
    conn.execute_batch("ANALYZE;").unwrap();
    check_path_form_plan(path_form_plan(&conn, sql)).unwrap();
    // Witnesses: the check fails when the lookup stops implying the index's
    // `WHERE`, and when the index is gone.
    let bypassed = sql.replace(" AND status = 'unresolved'", "");
    assert_ne!(bypassed, sql);
    assert!(check_path_form_plan(path_form_plan(&conn, &bypassed)).is_err());
    conn.execute_batch("DROP INDEX idx_refs_unresolved_last_segment;")
        .unwrap();
    assert!(check_path_form_plan(path_form_plan(&conn, sql)).is_err());
}

/// Name-match and type-match edges, in a stable order, as
/// `(caller file, call text, target file, target symbol, provenance)`.
fn dispatch_edges(store: &CallGraphStore) -> Vec<(String, String, String, String, String)> {
    let conn = store.conn.lock().unwrap();
    let mut statement = conn
        .prepare(
            "SELECT r.caller_file, r.full_ref, e.target_file, e.target_symbol, e.provenance
             FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE e.provenance IN (?1, ?2)
             ORDER BY 1, 2, 3, 4, 5",
        )
        .unwrap();
    statement
        .query_map(
            params![PROVENANCE_NAME_MATCH, PROVENANCE_TYPE_MATCH],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

const RUST_CARGO: (&str, &str) = (
    "Cargo.toml",
    "[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n",
);

// The edits below change what a type-aware resolver would infer for a caller
// in another file (a return type, a field type, a base type, a type's name or
// file) while the caller's own file stays unchanged. The `CallGraphStore`
// dispatch pass infers a receiver type only from the caller itself. In Rust
// that is `self`, a `Type::` path, the caller's own parameter types, and a
// direct `self.field` receiver whose struct and field type are declared in the
// caller's own file. In Java and Kotlin it is locals and fields of the
// enclosing type in the caller's own file, and in C++ declarations inside the
// enclosing function. Other languages fall back to name matching. A candidate
// is then chosen only from the methods sharing the call's name. The edges
// asserted unchanged below show the edit cannot move them; where edges do
// change, the caller is selected through the changed method names.

#[test]
fn s3_cross_file_return_type_change() {
    let fixture = Fixture::from_files(&[
        RUST_CARGO,
        ("lib.rs", "mod a; mod b;\n"),
        (
            "a.rs",
            "pub struct Foo;\npub struct Bar;\nimpl Foo { pub fn run(&self) {} }\nimpl Bar { pub fn run(&self) {} }\npub fn make() -> Foo { Foo }\n",
        ),
        (
            "b.rs",
            "use crate::a::make;\npub fn caller() { make().run(); let made = make(); made.run(); }\n",
        ),
    ]);
    let before = dispatch_edges(&fixture.store);
    let path = edit(
        &fixture.root,
        "a.rs",
        "pub fn make() -> Foo { Foo }",
        "pub fn make() -> Bar { Bar }",
    );
    fixture.refresh_and_compare("return-type", &[path]);
    assert_eq!(dispatch_edges(&fixture.store), before);
}

#[test]
fn s3_cross_file_field_type_change() {
    let fixture = Fixture::from_files(&[
        RUST_CARGO,
        ("lib.rs", "mod a; mod b;\n"),
        (
            "a.rs",
            "pub struct Foo;\npub struct Bar;\nimpl Foo { pub fn run(&self) {} }\nimpl Bar { pub fn run(&self) {} }\npub struct S { pub inner: Foo }\n",
        ),
        (
            "b.rs",
            "use crate::a::S;\npub fn caller(s: &S) { s.inner.run(); }\npub struct Local { inner: crate::a::S }\nimpl Local { pub fn go(&self) { self.inner.run(); } }\n",
        ),
    ]);
    let before = dispatch_edges(&fixture.store);
    let path = edit(&fixture.root, "a.rs", "inner: Foo", "inner: Bar");
    fixture.refresh_and_compare("field-type", &[path]);
    assert_eq!(dispatch_edges(&fixture.store), before);
}

#[test]
fn s3_cross_file_java_field_type_change() {
    // Java receiver inference reads fields of the caller's own class only.
    let fixture = Fixture::from_files(&[
        ("Foo.java", "class Foo { void run() {} }\n"),
        ("Bar.java", "class Bar { void run() {} }\n"),
        ("Holder.java", "class Holder { Foo inner; }\n"),
        (
            "Caller.java",
            "class Caller { Foo own; void go(Holder h) { h.inner.run(); own.run(); } }\n",
        ),
    ]);
    let before = dispatch_edges(&fixture.store);
    let path = edit(&fixture.root, "Holder.java", "Foo inner;", "Bar inner;");
    fixture.refresh_and_compare("java-field-type", &[path]);
    assert_eq!(dispatch_edges(&fixture.store), before);
}

#[test]
fn s3_cross_file_typescript_base_change() {
    let fixture = Fixture::from_files(&[
        (
            "bases.ts",
            "export class A { run(): void {} }\nexport class C { run(): void {} }\nexport class B extends A {}\n",
        ),
        (
            "caller.ts",
            "import { B } from './bases';\nexport function use(b: B): void { b.run(); }\n",
        ),
    ]);
    let before = dispatch_edges(&fixture.store);
    let path = edit(
        &fixture.root,
        "bases.ts",
        "class B extends A",
        "class B extends C",
    );
    fixture.refresh_and_compare("ts-base", &[path]);
    assert_eq!(dispatch_edges(&fixture.store), before);
}

#[test]
fn s3_cross_file_python_base_change() {
    let fixture = Fixture::from_files(&[
        (
            "bases.py",
            "class A:\n    def run(self):\n        pass\n\n\nclass C:\n    def run(self):\n        pass\n\n\nclass B(A):\n    pass\n",
        ),
        (
            "caller.py",
            "from bases import B\n\n\ndef use(b: B):\n    b.run()\n",
        ),
    ]);
    let before = dispatch_edges(&fixture.store);
    let path = edit(&fixture.root, "bases.py", "class B(A):", "class B(C):");
    fixture.refresh_and_compare("py-base", &[path]);
    assert_eq!(dispatch_edges(&fixture.store), before);
}

/// Adding `impl Speak for X` in another file adds the method
/// `Speak for X::speak`; the trait method name already exists in `Y`'s impl.
/// `x.speak()` has a known receiver type but no candidate scoped `X::speak`,
/// so it gets no edge either way. `any.speak()` has no known receiver type and
/// is name-matched: one `speak` candidate before the edit, two after.
const TRAIT_FILES: [(&str, &str); 5] = [
    RUST_CARGO,
    ("lib.rs", "mod traits; mod impls; mod caller;\n"),
    (
        "traits.rs",
        "pub trait Speak { fn speak(&self) {} }\npub struct X;\npub struct Y;\nimpl Speak for Y { fn speak(&self) {} }\npub fn pick() -> Y { Y }\n",
    ),
    ("impls.rs", "use crate::traits::{Speak, X};\npub fn other() {}\n"),
    (
        "caller.rs",
        "use crate::traits::{pick, Speak, X};\npub fn caller(x: &X) { x.speak(); let any = pick(); any.speak(); }\n",
    ),
];

const TRAIT_IMPL: &str = "impl Speak for X { fn speak(&self) {} }\n";

#[test]
fn s3_cross_file_rust_trait_impl_added() {
    let fixture = Fixture::from_files(&TRAIT_FILES);
    let before = dispatch_edges(&fixture.store);
    let path = edit(
        &fixture.root,
        "impls.rs",
        "pub fn other() {}\n",
        &format!("pub fn other() {{}}\n{TRAIT_IMPL}"),
    );
    let stats = fixture.refresh_and_compare("trait-impl-added", &[path]);
    assert!(stats.dispatch_callers_recomputed.contains("caller.rs"));
    assert_ne!(dispatch_edges(&fixture.store), before);
}

#[test]
fn s3_cross_file_rust_trait_impl_removed() {
    let fixture = Fixture::from_files(&TRAIT_FILES);
    let original = dispatch_edges(&fixture.store);
    let path = edit(
        &fixture.root,
        "impls.rs",
        "pub fn other() {}\n",
        &format!("pub fn other() {{}}\n{TRAIT_IMPL}"),
    );
    fixture.refresh_and_compare("trait-impl-setup", std::slice::from_ref(&path));
    assert_ne!(dispatch_edges(&fixture.store), original);
    edit(&fixture.root, "impls.rs", TRAIT_IMPL, "");
    let stats = fixture.refresh_and_compare("trait-impl-removed", &[path]);
    assert!(stats.dispatch_callers_recomputed.contains("caller.rs"));
    assert_eq!(dispatch_edges(&fixture.store), original);
}

const TYPE_FILES: [(&str, &str); 5] = [
    RUST_CARGO,
    ("lib.rs", "mod a; mod c; mod b;\n"),
    (
        "a.rs",
        "pub struct Foo;\nimpl Foo { pub fn run(&self) {} }\n",
    ),
    ("c.rs", "pub fn unrelated() {}\n"),
    (
        "b.rs",
        "use crate::a::Foo;\npub fn caller(x: &Foo) { x.run(); }\n",
    ),
];

fn type_target(store: &CallGraphStore) -> Vec<(String, String)> {
    dispatch_edges(store)
        .into_iter()
        .filter(|(caller, ..)| caller == "b.rs")
        .map(|(_, _, file, symbol, _)| (file, symbol))
        .collect()
}

#[test]
fn s3_cross_file_type_renamed() {
    let fixture = Fixture::from_files(&TYPE_FILES);
    assert_eq!(
        type_target(&fixture.store),
        vec![("a.rs".to_string(), "Foo::run".to_string())]
    );
    fs::write(
        fixture.root.join("a.rs"),
        "pub struct Baz;\nimpl Baz { pub fn run(&self) {} }\n",
    )
    .unwrap();
    let stats = fixture.refresh_and_compare("type-renamed", &[fixture.root.join("a.rs")]);
    assert!(stats.dispatch_callers_recomputed.contains("b.rs"));
    assert_eq!(type_target(&fixture.store), Vec::new());
}

#[test]
fn s3_cross_file_type_moved_between_files() {
    let fixture = Fixture::from_files(&TYPE_FILES);
    let from = fixture.root.join("a.rs");
    let to = fixture.root.join("c.rs");
    fs::write(&from, "pub fn emptied() {}\n").unwrap();
    fs::write(
        &to,
        "pub fn unrelated() {}\npub struct Foo;\nimpl Foo { pub fn run(&self) {} }\n",
    )
    .unwrap();
    let stats = fixture.refresh_and_compare("type-moved", &[from, to]);
    assert!(stats.dispatch_callers_recomputed.contains("b.rs"));
    assert_eq!(
        type_target(&fixture.store),
        vec![("c.rs".to_string(), "Foo::run".to_string())]
    );
}

/// Checks that every name-match and type-match edge is found by the name
/// lookups `dispatch_callers_to_recompute` runs when its target method's
/// candidates change: the call's short name equals the method name, or the
/// call is a path-form call whose last segment does and the method's file is
/// not JS/TS (only non-JS/TS changes run the path-form lookup). Returns the
/// number of edges checked and how many only the path-form lookup finds.
fn check_edges_found_by_name_lookups(
    store: &CallGraphStore,
) -> std::result::Result<(usize, usize), String> {
    let conn = store.conn.lock().unwrap();
    let mut statement = conn
        .prepare(
            "SELECT r.full_ref, r.short_name, r.callee_last_segment, n.name, n.file_path
             FROM edges e JOIN refs r ON r.ref_id = e.ref_id JOIN nodes n ON n.id = e.target_node
             WHERE e.provenance IN (?1, ?2)",
        )
        .unwrap();
    let rows = statement
        .query_map(
            params![PROVENANCE_NAME_MATCH, PROVENANCE_TYPE_MATCH],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut path_form_only = 0;
    for (full_ref, short_name, last_segment, name, file) in &rows {
        if short_name.as_deref() == Some(name.as_str()) {
            continue;
        }
        let js_family = Path::new(file)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| JS_TS_EXTENSIONS.contains(&ext));
        if last_segment.as_deref() == Some(name.as_str()) && !js_family {
            path_form_only += 1;
            continue;
        }
        return Err(format!(
            "edge from {full_ref:?} (short name {short_name:?}) to {file}:{name} is not found by name"
        ));
    }
    Ok((rows.len(), path_form_only))
}

#[test]
fn s3_every_dispatch_edge_is_found_by_the_name_lookups() {
    let fixture = Fixture::new(10, 10);
    let (edges, _) = check_edges_found_by_name_lookups(&fixture.store).unwrap();
    assert!(edges >= 10, "{edges}");
    // Inherent constructors bind during ordinary resolution. Trait-associated
    // functions still exercise supplemental path-form lookup through a real impl.
    let path_forms = Fixture::from_files(&[(
        "path.rs",
        "pub trait PathFactory { fn build() -> Self; }\npub struct PathTarget;\nimpl PathFactory for PathTarget { fn build() -> Self { Self } }\nfn first() { PathTarget::build(); }\nfn second() { PathTarget::build(); }\n",
    )]);
    let (path_edges, path_form_only) =
        check_edges_found_by_name_lookups(&path_forms.store).unwrap();
    assert_eq!(path_edges, 2);
    assert_eq!(path_form_only, 2);

    // Receiver shapes across the languages with method dispatch.
    let languages = Fixture::from_files(&[
        (
            "a.rs",
            "pub struct Foo; impl Foo { pub fn run(&self) {} }\nfn f(x: &Foo, y: Option<Foo>, s: S) { x.run::<u8>(); Foo::<u8>::run(); (*x).run(); x.clone().run(); y.unwrap().run(); s.inner.run(); <Foo as T>::run(x); }\n",
        ),
        (
            "b.ts",
            "export class Box { run(): void {} }\nexport function g(x: any) { x?.run(); x!.run(); new Box().run(); x.a.b.run(); (x as Box).run(); }\n",
        ),
        (
            "c.py",
            "class K:\n    def run(self):\n        pass\n\n\ndef h(x):\n    x.y.run()\n    super().run()\n    K().run()\n",
        ),
        (
            "d.cpp",
            "struct S { void run(); };\nvoid k(S* p, S s) { p->run(); s.run(); S::run(); s.template run<int>(); (*p).run(); }\n",
        ),
        (
            "e.java",
            "class J { void run() {} void m(J o) { o.run(); this.run(); new J().run(); } }\n",
        ),
        (
            "f.go",
            "package f\ntype G struct{}\nfunc (g G) Run() {}\nfunc m(g G) { g.Run(); (&g).Run() }\n",
        ),
    ]);
    let (edges, _) = check_edges_found_by_name_lookups(&languages.store).unwrap();
    assert!(edges >= 10, "{edges}");

    // Witness: an edge whose call no longer carries the method name fails.
    fixture
        .store
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE refs SET short_name = 'renamed', callee_last_segment = 'renamed'
             WHERE ref_id = (SELECT ref_id FROM edges WHERE provenance = 'name_match' LIMIT 1)",
            [],
        )
        .unwrap();
    assert!(check_edges_found_by_name_lookups(&fixture.store).is_err());
}

/// The probe edit of the call-graph incremental-speed study: a leading comment
/// that shifts every line, plus a new uniquely named caller/callee pair at the
/// end of the file.
fn speed_probe_edit(original: &str, tag: &str) -> String {
    let caller = format!("aft_speed_probe_caller_{tag}");
    let callee = format!("aft_speed_probe_callee_{tag}");
    let mut text = format!("// aft speed probe {tag}\n{original}");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "\npub fn {caller}() -> usize {{\n    {callee}()\n}}\npub fn {callee}() -> usize {{\n    {}\n}}\n",
        tag.len()
    ));
    text
}

fn host_load() -> String {
    fs::read_to_string("/proc/loadavg")
        .map(|load| load.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// `CallGraphStore` refresh phases for the probe edit on two Rust hub files of a real
/// repository. `AFT_S3_MEASURE_ROOT` names a git checkout whose tracked
/// sources are copied; `AFT_S3_MEASURE_ROUNDS` (default 3) sets the edits per
/// file. Each edit is undone with an unmeasured refresh before the next.
#[test]
#[ignore = "offline measurement: set AFT_S3_MEASURE_ROOT to a git checkout"]
fn s3_offline_hub_refresh_measurement() {
    let source = PathBuf::from(std::env::var_os("AFT_S3_MEASURE_ROOT").expect("source checkout"));
    let rounds = std::env::var("AFT_S3_MEASURE_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3usize);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    perf_tests::copy_tracked_sources(&source, &root);
    let files: Vec<_> = callgraph::walk_project_files(&root).collect();
    let store = CallGraphStore::open(temp.path().join("store"), root.clone()).unwrap();
    let started = Instant::now();
    store.cold_build(&files).unwrap();
    eprintln!(
        "S3 measurement cold build: files={} ms={} load={}",
        files.len(),
        started.elapsed().as_millis(),
        host_load()
    );
    for round in 0..rounds {
        for rel in ["crates/aft/src/protocol.rs", "crates/aft/src/context.rs"] {
            let path = root.join(rel);
            let original = fs::read_to_string(&path).unwrap();
            fs::write(&path, speed_probe_edit(&original, &format!("s3r{round}"))).unwrap();
            store.checkpoint_wal_truncate();
            let load = host_load();
            let (stats, profile) = store
                .refresh_files_profiled(std::slice::from_ref(&path))
                .unwrap();
            eprintln!(
                "S3 measurement {rel} round={round} load_before=[{load}]: {} dependency_selected_refs={} refreshed_own_files={} projection_callers={} dispatch_callers_recomputed={} dispatch_path_form_rows_examined={}",
                profile.report(),
                stats.dependency_selected_refs,
                stats.refreshed_own_files,
                stats.projection_callers.len(),
                stats.dispatch_callers_recomputed.len(),
                stats.dispatch_path_form_rows_examined,
            );
            fs::write(&path, original).unwrap();
            store.refresh_files(std::slice::from_ref(&path)).unwrap();
        }
    }
}

/// Runs [`check_edges_found_by_name_lookups`] over a cold build of a real
/// repository (`AFT_S3_MEASURE_ROOT`, a git checkout whose tracked sources are
/// copied).
#[test]
#[ignore = "offline census: set AFT_S3_MEASURE_ROOT to a git checkout"]
fn s3_offline_dispatch_edge_name_census() {
    let source = PathBuf::from(std::env::var_os("AFT_S3_MEASURE_ROOT").expect("source checkout"));
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    perf_tests::copy_tracked_sources(&source, &root);
    let files: Vec<_> = callgraph::walk_project_files(&root).collect();
    let store = CallGraphStore::open(temp.path().join("store"), root).unwrap();
    store.cold_build(&files).unwrap();
    let (edges, path_form_only) = check_edges_found_by_name_lookups(&store).unwrap();
    eprintln!(
        "S3 census: files={} dispatch_edges={edges} found_only_by_path_form={path_form_only}",
        files.len()
    );
    assert!(edges > 0);
}
