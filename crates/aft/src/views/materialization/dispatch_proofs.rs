use super::*;
use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};

fn manifest(connection: &Connection, types: &str) -> Manifest {
    // Dispatch assertions read these blobs after setup; batch fixture writes
    // rather than paying for a durable commit for each input on Windows.
    let tx = connection.unchecked_transaction().unwrap();
    let inputs = [("types.ts", types), ("caller.ts", "import { I } from './types'; export function caller(x: I) { x.m(); }\nexport function unknown(x) { x.m(); }")];
    let manifest = Manifest::new(inputs.into_iter().map(|(path, source)| {
        let payload = join::CallgraphBlob::extract(source, "typescript", "dispatch-proof")
            .unwrap()
            .to_bytes()
            .unwrap();
        let key = blake3::hash(&payload).to_hex().to_string();
        tx.execute(
            "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
            params![
                decode_manifest_full_key(&key).unwrap(),
                payload,
                blake3::hash(&payload).as_bytes().as_slice()
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
    assert!(
        !tx.is_autocommit(),
        "fixture payloads must share one transaction"
    );
    tx.commit().unwrap();
    manifest
}
fn blobs(path: &Path) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB NOT NULL, payload_digest BLOB NOT NULL, payload_schema INTEGER NOT NULL)",
        )
        .unwrap();
    connection
}

#[test]
fn seeded_dispatch_incremental_equals_independent_cold_through_switches() {
    let dir = tempfile::tempdir().unwrap();
    let blob_path = dir.path().join("blobs.sqlite");
    let connection = blobs(&blob_path);
    let states = [
        "export interface I { m(): void; }\nexport class A implements I { m() {} }",
        "export interface I { m(): void; }\nexport class A implements I { m() {} }\nexport class B implements I { private m(a: number) {} n() {} }",
        "export interface I { m(): void; }\nexport class B implements I { m() {} }",
    ];
    let manifests = states
        .iter()
        .map(|source| manifest(&connection, source))
        .collect::<Vec<_>>();
    drop(connection);
    let seed = dir.path().join("seed.sqlite");
    materialize_manifest_view_database(&seed, &blob_path, &manifests[0]).unwrap();
    let incremental = dir.path().join("incremental.sqlite");
    crate::views::generation::clone_derived_for_test(&seed, &incremental).unwrap();
    let mut previous = 0;
    for (step, next) in [1, 2, 0, 2, 1, 0].into_iter().enumerate() {
        apply_manifest_diff(
            &incremental,
            &manifests[previous],
            &manifests[next],
            &blob_path,
        )
        .unwrap();
        // The oracle extracts from source into an empty store, not the tested
        // view's blobs, seed or derived database.
        let cold_blobs = dir.path().join(format!("cold-blobs-{step}.sqlite"));
        let connection = blobs(&cold_blobs);
        let cold_manifest = manifest(&connection, states[next]);
        drop(connection);
        let cold = dir.path().join(format!("cold-{step}.sqlite"));
        materialize_manifest_view_database(&cold, &cold_blobs, &cold_manifest).unwrap();
        assert_eq!(
            parity::logical_snapshot(&incremental),
            parity::logical_snapshot(&cold)
        );
        let connection = Connection::open(&incremental).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM edges WHERE provenance='dispatch'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            if next == 1 { 2 } else { 1 }
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM view_unknown_live", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            if next == 1 { 3 } else { 2 }
        );
        previous = next;
    }
}

struct PauseReader<'a> {
    inner: ManifestViewBlobReader<'a>,
    marker: Option<std::path::PathBuf>,
}
impl join::ManifestBlobReader for PauseReader<'_> {
    fn read_callgraph_blob(
        &self,
        key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, join::ManifestJoinError> {
        if let Some(marker) = &self.marker {
            // The materializer has already begun its write transaction and
            // deleted old rows before its first read. Kill here proves rollback.
            std::fs::write(marker, b"transaction-in-progress").unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.inner.read_callgraph_blob(key)
    }
}
#[test]
fn derived_publication_child() {
    let Some(directory) = std::env::var_os("AFT_DISPATCH_PUBLICATION_CHILD") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let manifest =
        Manifest::from_json_bytes(&std::fs::read(directory.join("manifest.json")).unwrap())
            .unwrap();
    let connection = Connection::open(directory.join("blobs.sqlite")).unwrap();
    let reader = PauseReader {
        inner: ManifestViewBlobReader::new(&connection),
        marker: std::env::var_os("AFT_DISPATCH_PAUSE").map(std::path::PathBuf::from),
    };
    materialize_from_blob_reader(&directory.join("derived.sqlite"), &manifest, &reader).unwrap();
}
#[test]
fn kill_during_derived_publication_rolls_back_and_real_restart_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let blob_path = dir.path().join("blobs.sqlite");
    let connection = blobs(&blob_path);
    let old = manifest(
        &connection,
        "export interface I { m(): void; }\nexport class A implements I { m() {} }",
    );
    let next = manifest(&connection, "export interface I { m(): void; }\nexport class A implements I { m() {} }\nexport class B implements I { m() {} }");
    drop(connection);
    let derived = dir.path().join("derived.sqlite");
    materialize_manifest_view_database(&derived, &blob_path, &old).unwrap();
    let before = parity::logical_snapshot(&derived);
    std::fs::write(
        dir.path().join("manifest.json"),
        next.to_json_bytes().unwrap(),
    )
    .unwrap();
    let child = || {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "views::materialization::dispatch_proofs::derived_publication_child",
                "--nocapture",
            ])
            .env("AFT_DISPATCH_PUBLICATION_CHILD", dir.path());
        command
    };
    let marker = dir.path().join("paused");
    let mut killed = child().env("AFT_DISPATCH_PAUSE", &marker).spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child did not reach materialization transaction"
        );
        assert!(
            killed.try_wait().unwrap().is_none(),
            "child exited before pause"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    killed.kill().unwrap();
    assert!(!killed.wait().unwrap().success());
    assert_eq!(
        parity::logical_snapshot(&derived),
        before,
        "kill must preserve the old fully committed graph"
    );
    assert!(
        child().status().unwrap().success(),
        "new process must rebuild after kill"
    );
    let cold = dir.path().join("cold.sqlite");
    materialize_manifest_view_database(&cold, &blob_path, &next).unwrap();
    assert_eq!(
        parity::logical_snapshot(&derived),
        parity::logical_snapshot(&cold)
    );
    assert!(
        child().status().unwrap().success(),
        "second real exit/restart must be idempotent"
    );
    assert_eq!(
        parity::logical_snapshot(&derived),
        parity::logical_snapshot(&cold)
    );
}

#[test]
fn public_unknown_dynamic_counts_and_liveness_projection() {
    for (language, source) in [
        ("typescript", "class A { m() {} }\nclass B { private m(a: number) {} n() {} }\nfunction caller(x, name) { x.m(); x[name](); }"),
        ("javascript", "class A { m() {} }\nclass B { m(a) {} n() {} }\nfunction caller(x, name) { x.m(); x[name](); }"),
        ("python", "class A:\n def m(self): pass\nclass B:\n def m(self, a): pass\n def n(self): pass\ndef caller(x, name):\n x.m()\n getattr(x, name)()\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let payload = join::CallgraphBlob::extract(source, language, "dispatch-proof").unwrap().to_bytes().unwrap();
        let key = blake3::hash(&payload).to_hex().to_string();
        let blob_path = dir.path().join("blobs.sqlite");
        let connection = blobs(&blob_path);
        connection.execute("INSERT INTO blob_payloads VALUES (?1, ?2, ?3, 1)", params![decode_manifest_full_key(&key).unwrap(), payload, blake3::hash(&payload).as_bytes().as_slice()]).unwrap();
        drop(connection);
        let manifest = Manifest::new([(RelPath::new(b"fixture").unwrap(), ManifestEntry::Regular { mode: 0o100644, planes: RegularPlanes { callgraph: Some(key), semantic: None }, resolution_input: false })]).unwrap();
        let views = crate::views::ViewStore::open_dir(dir.path().join("view")).unwrap();
        let database = views.derived_path("unknown-counts").unwrap();
        materialize_manifest_view_database(&database, &blob_path, &manifest).unwrap();
        let reader = crate::callgraph_store::ReadonlyCallGraphStore::open_pinned_derived(dir.path().to_path_buf(), "family".into(), views.view_dir().to_path_buf(), "unknown-counts").unwrap();
        let counts = reader.dispatch_site_counts().unwrap().unwrap();
        assert_eq!(counts[language], dispatch::SiteCounts { unresolved_receiver_sites: 1, dynamic_member_sites: 1, external: 0 });
        for excluded in ["rust", "go", "java", "csharp", "kotlin"] { assert_eq!(counts[excluded].dynamic_member_sites, 0); }
        assert_eq!(reader.edge_snapshot().unwrap().len(), 0, "unknown/dynamic must not leak caller/impact/trace edges");
        let (_, snapshot, _, _) = crate::callgraph_store::project_dead_code_snapshot_from_view(&reader).unwrap();
        let protected = snapshot.entry_point_symbols.values().flatten().collect::<std::collections::BTreeSet<_>>();
        assert!(protected.iter().any(|s| s.as_str() == "A::m"));
        assert!(protected.iter().any(|s| s.as_str() == "B::m"));
        assert!(!protected.iter().any(|s| s.ends_with("::n")));
    }
}

/// Stores each source as a callgraph blob and returns the manifest naming them.
fn manifest_of(connection: &Connection, files: &[(&str, &str)]) -> Manifest {
    Manifest::new(files.iter().map(|(path, source)| {
        let language = if path.ends_with(".rs") {
            "rust"
        } else {
            "typescript"
        };
        let payload = join::CallgraphBlob::extract(source, language, "dispatch-proof")
            .unwrap()
            .to_bytes()
            .unwrap();
        let key = blake3::hash(&payload).to_hex().to_string();
        connection
            .execute(
                "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
                params![
                    decode_manifest_full_key(&key).unwrap(),
                    payload,
                    blake3::hash(&payload).as_bytes().as_slice()
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
    .unwrap()
}

/// Applies `base -> next` to a cold build of `base` and requires every derived
/// table to equal a cold build of `next`. Returns the patched database.
fn incremental_matches_cold(
    dir: &Path,
    base: &[(&str, &str)],
    next: &[(&str, &str)],
) -> (std::path::PathBuf, MaterializeStats) {
    let blob_path = dir.join("blobs.sqlite");
    let connection = blobs(&blob_path);
    let base = manifest_of(&connection, base);
    let next = manifest_of(&connection, next);
    drop(connection);
    let incremental = dir.join("incremental.sqlite");
    materialize_manifest_view_database(&incremental, &blob_path, &base).unwrap();
    let stats = apply_manifest_diff(&incremental, &base, &next, &blob_path).unwrap();
    let cold = dir.join("cold.sqlite");
    materialize_manifest_view_database(&cold, &blob_path, &next).unwrap();
    parity::assert_logical_parity(&incremental, &cold);
    (incremental, stats)
}

fn count(database: &Path, sql: &str) -> usize {
    Connection::open(database)
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

const INTERFACE: &str = "export interface I { m(): void; }";
const CALLER: &str = "import { I } from './types';\nexport function caller(x: I) { x.m(); }";
const IMPLEMENTATION: &str = "import { I } from './types';\nexport class B implements I { m() {} }";

#[test]
fn relinked_unchanged_caller_gets_method_call_status_and_edges_back() {
    // Removing `helper` changes an answer the unchanged caller consulted, so
    // it is re-resolved and ordinary emission rewrites its method-call site
    // ref (status `unresolved`) and drops that ref's method-call edge. The
    // pass must restore both.
    let caller =
        "import { I, helper } from './types';\nexport function caller(x: I) { helper(); x.m(); }";
    let dir = tempfile::tempdir().unwrap();
    let (database, stats) = incremental_matches_cold(
        dir.path(),
        &[
            (
                "types.ts",
                &format!("{INTERFACE}\nexport function helper() {{}}"),
            ),
            ("caller.ts", caller),
            ("impl.ts", IMPLEMENTATION),
        ],
        &[
            ("types.ts", INTERFACE),
            ("caller.ts", caller),
            ("impl.ts", IMPLEMENTATION),
        ],
    );
    assert!(
        stats.dependent_files >= 1 && stats.relinked_inserted >= 1,
        "the caller must be re-resolved and relinked: {stats:?}"
    );
    assert_eq!(
        count(
            &database,
            "SELECT COUNT(*) FROM refs WHERE caller_file='caller.ts' AND status='resolved'
             AND ref_id IN (SELECT ref_id FROM edges WHERE provenance='dispatch')"
        ),
        1
    );
}

#[test]
fn shifted_target_ordinals_relink_method_call_edges_of_unchanged_callers() {
    // The caller does not depend on impl.ts, so it is not re-resolved, but its
    // method-call edge points at a node whose ID changes with the ordinal.
    let dir = tempfile::tempdir().unwrap();
    let (database, stats) = incremental_matches_cold(
        dir.path(),
        &[
            ("types.ts", INTERFACE),
            ("caller.ts", CALLER),
            ("impl.ts", IMPLEMENTATION),
        ],
        &[
            ("types.ts", INTERFACE),
            ("caller.ts", CALLER),
            (
                "impl.ts",
                &format!(
                    "{}\nexport function first() {{}}\nexport class B implements I {{ m() {{}} }}",
                    "import { I } from './types';"
                ),
            ),
        ],
    );
    assert_eq!(
        stats.resolved_files, 1,
        "only impl.ts re-resolves: {stats:?}"
    );
    assert_eq!(
        count(
            &database,
            "SELECT COUNT(*) FROM edges WHERE provenance='dispatch' AND target_file='impl.ts'
             AND target_node IN (SELECT id FROM nodes)"
        ),
        1
    );
}

#[test]
fn removed_file_drops_its_method_call_site_rows() {
    let dir = tempfile::tempdir().unwrap();
    let extra = "import { I } from './types';\nexport function g(x: I) { x.m(); }\nexport function h(y) { y.m(); }";
    let files = [
        ("types.ts", INTERFACE),
        ("caller.ts", CALLER),
        ("impl.ts", IMPLEMENTATION),
    ];
    let mut base = files.to_vec();
    base.push(("extra.ts", extra));
    let (database, _) = incremental_matches_cold(dir.path(), &base, &files);
    assert_eq!(
        count(
            &database,
            "SELECT COUNT(*) FROM view_dispatch_sites WHERE caller_file='extra.ts'"
        ),
        0
    );
    assert_eq!(
        count(&database, "SELECT COUNT(*) FROM view_dispatch_sites"),
        1
    );
}

/// Deletes the node of the function `caller` defined in `caller.ts`, then runs
/// only the method-call pass on `database`, as if `caller.ts` had been
/// rewritten without that node.
fn emit_without_caller_node(
    database: &Path,
    blob_path: &Path,
    manifest: &Manifest,
    row_by_row: bool,
) {
    let mut connection = Connection::open(database).unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "DELETE FROM nodes WHERE file_path='caller.ts' AND scoped_name='caller'",
            [],
        )
        .unwrap();
    if row_by_row {
        // Makes the row-by-row writer start from empty site tables and
        // method-call edges, as in a cold build.
        transaction
            .execute(
                "UPDATE meta SET v='stale' WHERE k='view_dispatch_signature'",
                [],
            )
            .unwrap();
    }
    let blobs = Connection::open(blob_path).unwrap();
    let reader = ManifestViewBlobReader::new(&blobs);
    let changed = BTreeSet::from([b"caller.ts".to_vec()]);
    dispatch::WRITE_ROW_BY_ROW.with(|cell| cell.set(row_by_row));
    let emitted = dispatch::emit(
        &transaction,
        manifest,
        &reader,
        &dispatch::EmitScope {
            changed_paths: Some(&changed),
            ordinary_edge_refs: &HashSet::new(),
            rewritten_ref_files: &BTreeSet::new(),
            type_targets: &BTreeMap::new(),
            reuse: false,
        },
    );
    dispatch::WRITE_ROW_BY_ROW.with(|cell| cell.set(false));
    emitted.unwrap();
    transaction.commit().unwrap();
}

#[test]
fn site_whose_caller_has_no_node_gets_no_method_call_edge() {
    let dir = tempfile::tempdir().unwrap();
    let blob_path = dir.path().join("blobs.sqlite");
    let connection = blobs(&blob_path);
    let manifest = manifest_of(
        &connection,
        &[
            ("types.ts", INTERFACE),
            ("caller.ts", CALLER),
            ("impl.ts", IMPLEMENTATION),
        ],
    );
    drop(connection);
    let base = dir.path().join("base.sqlite");
    materialize_manifest_view_database(&base, &blob_path, &manifest).unwrap();
    let from_caller =
        "SELECT COUNT(*) FROM edges WHERE provenance='dispatch' AND ref_id LIKE 'view:caller.ts:%'";
    assert_eq!(
        count(&base, from_caller),
        1,
        "the caller starts with an edge"
    );
    let mut databases = Vec::new();
    for row_by_row in [false, true] {
        let database = dir
            .path()
            .join(format!("without-caller-{row_by_row}.sqlite"));
        crate::views::generation::clone_derived_for_test(&base, &database).unwrap();
        emit_without_caller_node(&database, &blob_path, &manifest, row_by_row);
        databases.push(database);
    }
    parity::assert_logical_parity(&databases[0], &databases[1]);
    assert_eq!(count(&databases[0], from_caller), 0);
    assert_eq!(
        count(
            &databases[0],
            "SELECT COUNT(*) FROM view_dispatch_sites WHERE caller_file='caller.ts'"
        ),
        1
    );
}

/// Today's joins never resolve a reference at a method-call site's ordinal, so
/// ordinary emission never writes an edge on a site's ref ID. This test plants
/// such an edge, as a changed join could, and requires the pass to delete it
/// exactly like the row-by-row writer does.
#[test]
fn ordinary_edge_on_a_method_call_site_ref_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let blob_path = dir.path().join("blobs.sqlite");
    let connection = blobs(&blob_path);
    let manifest = manifest_of(
        &connection,
        &[
            ("types.ts", INTERFACE),
            ("caller.ts", CALLER),
            ("impl.ts", IMPLEMENTATION),
        ],
    );
    drop(connection);
    let base = dir.path().join("base.sqlite");
    materialize_manifest_view_database(&base, &blob_path, &manifest).unwrap();
    let ref_id: String = Connection::open(&base)
        .unwrap()
        .query_row(
            "SELECT 'view:' || caller_file || ':' || ordinal FROM view_dispatch_sites
             WHERE caller_file='caller.ts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let ordinary = format!(
        "SELECT COUNT(*) FROM edges WHERE ref_id='{ref_id}' AND provenance='{PROVENANCE_TREESITTER}'"
    );
    let mut databases = Vec::new();
    for row_by_row in [false, true] {
        let database = dir
            .path()
            .join(format!("ordinary-edge-{row_by_row}.sqlite"));
        crate::views::generation::clone_derived_for_test(&base, &database).unwrap();
        let mut connection = Connection::open(&database).unwrap();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO edges (edge_id, ref_id, source_node, target_node, target_file,
                                    target_symbol, kind, line, provenance)
                 SELECT 'edge:' || ?1, ?1, id, id, file_path, scoped_name, 'call', 2, ?2
                 FROM nodes WHERE file_path='caller.ts' AND scoped_name='caller'",
                params![ref_id, PROVENANCE_TREESITTER],
            )
            .unwrap();
        assert_eq!(count_in(&transaction, &ordinary), 1, "the edge is planted");
        if row_by_row {
            transaction
                .execute(
                    "UPDATE meta SET v='stale' WHERE k='view_dispatch_signature'",
                    [],
                )
                .unwrap();
        }
        let blobs = Connection::open(&blob_path).unwrap();
        let reader = ManifestViewBlobReader::new(&blobs);
        let changed = BTreeSet::new();
        let planted = HashSet::from([ref_id.clone()]);
        dispatch::WRITE_ROW_BY_ROW.with(|cell| cell.set(row_by_row));
        let emitted = dispatch::emit(
            &transaction,
            &manifest,
            &reader,
            &dispatch::EmitScope {
                changed_paths: Some(&changed),
                ordinary_edge_refs: &planted,
                rewritten_ref_files: &BTreeSet::new(),
                type_targets: &BTreeMap::new(),
                reuse: false,
            },
        );
        dispatch::WRITE_ROW_BY_ROW.with(|cell| cell.set(false));
        emitted.unwrap();
        transaction.commit().unwrap();
        databases.push(database);
    }
    assert_eq!(count(&databases[0], &ordinary), 0);
    parity::assert_logical_parity(&databases[0], &databases[1]);
}

fn count_in(connection: &Connection, sql: &str) -> usize {
    connection.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// A cold build written by differences must equal the row-by-row writer, for
/// TypeScript interface and unknown receivers and for a Rust macro whose
/// method call shares its AST ordinal with an ordinary call.
#[test]
fn differences_writer_matches_row_by_row_cold_builds() {
    let fixtures: [&[(&str, &str)]; 2] = [
        &[
            ("types.ts", INTERFACE),
            ("caller.ts", CALLER),
            ("impl.ts", IMPLEMENTATION),
            ("unknown.ts", "export function h(y) { y.m(); }"),
        ],
        &[(
            "lib.rs",
            "struct S;\nimpl S { fn m(&self) -> i32 { 1 } }\nfn helper() -> i32 { 2 }\nfn caller(s: &S) { assert_eq!(s.m(), helper()); }\n",
        )],
    ];
    for (index, files) in fixtures.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let blob_path = dir.path().join("blobs.sqlite");
        let connection = blobs(&blob_path);
        let manifest = manifest_of(&connection, files);
        drop(connection);
        let differences = dir.path().join("differences.sqlite");
        materialize_manifest_view_database(&differences, &blob_path, &manifest).unwrap();
        dispatch::WRITE_ROW_BY_ROW.with(|row_by_row| row_by_row.set(true));
        let row_by_row = dir.path().join("row-by-row.sqlite");
        let built = materialize_manifest_view_database(&row_by_row, &blob_path, &manifest);
        dispatch::WRITE_ROW_BY_ROW.with(|row_by_row| row_by_row.set(false));
        built.unwrap();
        parity::assert_logical_parity(&differences, &row_by_row);
        assert!(
            count(&differences, "SELECT COUNT(*) FROM view_dispatch_sites") > 0,
            "fixture {index} has method-call sites"
        );
    }
}
