use super::*;
use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};

fn manifest(connection: &Connection, types: &str) -> Manifest {
    let inputs = [("types.ts", types), ("caller.ts", "import { I } from './types'; export function caller(x: I) { x.m(); }\nexport function unknown(x) { x.m(); }")];
    Manifest::new(inputs.into_iter().map(|(path, source)| {
        let payload = join::CallgraphBlob::extract(source, "typescript", "dispatch-proof")
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
    std::fs::copy(&seed, &incremental).unwrap();
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
