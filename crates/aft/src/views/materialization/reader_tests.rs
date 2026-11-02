use super::*;
use crate::blob_store::v2::{FamilyKey, FamilyPlane};
use crate::blob_store::v3::{publish, PlaneReader};
use crate::views::callgraph::V3BlobReader;

// Copy rows through the fixture's owning SQLite connection. The destination
// uses the v3 publication API so its PlaneReader shares the publication's owner.
pub(crate) fn v3_reader(connection: &Connection, storage: &Path) -> V3BlobReader {
    let rows = connection
        .prepare("SELECT full_key, payload FROM blob_payloads")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let request = rows.iter().map(|(_, payload)| payload.len() as u64).sum();
    publish(storage, "reader-test", request, None, |publication| {
        for (key, payload) in &rows {
            publication.put(
                &FamilyKey::new(FamilyPlane::Callgraph, key.as_slice().try_into().unwrap()),
                payload,
                crate::blob_store::CALLGRAPH_PAYLOAD_SCHEMA,
            )?;
        }
        Ok(())
    })
    .unwrap();
    V3BlobReader::new(
        PlaneReader::open_existing(storage, "reader-test", FamilyPlane::Callgraph).unwrap(),
    )
}

pub(crate) fn assert_query_parity(
    actual: &Path,
    expected: &Path,
    root: &Path,
    queries: &[(&str, &str)],
) {
    let open = |path: &Path| {
        let generation = path
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("derived-")
            .unwrap();
        crate::callgraph_store::ReadonlyCallGraphStore::open_pinned_derived(
            root.to_path_buf(),
            "reader-test".into(),
            path.parent().unwrap().to_path_buf(),
            generation,
        )
        .unwrap()
    };
    let (actual, expected) = (open(actual), open(expected));
    for &(file, symbol) in queries {
        let file = Path::new(file);
        macro_rules! compare_answer {
            ($operation:ident) => {
                assert_eq!(
                    serde_json::to_vec(
                        &crate::commands::callgraph_store_adapter::$operation(
                            &actual, file, symbol, 3, true
                        )
                        .unwrap()
                    )
                    .unwrap(),
                    serde_json::to_vec(
                        &crate::commands::callgraph_store_adapter::$operation(
                            &expected, file, symbol, 3, true
                        )
                        .unwrap()
                    )
                    .unwrap(),
                    "{} {file:?} {symbol}",
                    stringify!($operation)
                );
            };
        }
        compare_answer!(callers_result);
        compare_answer!(impact_result);
        compare_answer!(call_tree_result);
        let sources = [(file.to_str().unwrap().to_string(), symbol.to_string())];
        assert_eq!(
            format!("{:?}", actual.outgoing_calls_for_symbols(&sources).unwrap()).as_bytes(),
            format!(
                "{:?}",
                expected.outgoing_calls_for_symbols(&sources).unwrap()
            )
            .as_bytes(),
            "batched callees {file:?} {symbol}"
        );
        assert_eq!(
            format!("{:?}", actual.direct_callers_for_symbols(&sources).unwrap()).as_bytes(),
            format!(
                "{:?}",
                expected.direct_callers_for_symbols(&sources).unwrap()
            )
            .as_bytes(),
            "batched callers {file:?} {symbol}"
        );
        assert_eq!(
            format!("{:?}", actual.callers_of(file, symbol, 3).unwrap()).as_bytes(),
            format!("{:?}", expected.callers_of(file, symbol, 3).unwrap()).as_bytes(),
            "callers {file:?} {symbol}"
        );
        assert_eq!(
            format!("{:?}", actual.impact_of(file, symbol, 3).unwrap()).as_bytes(),
            format!("{:?}", expected.impact_of(file, symbol, 3).unwrap()).as_bytes(),
            "impact {file:?} {symbol}"
        );
        assert_eq!(
            serde_json::to_vec(&actual.call_tree(file, symbol, 3).unwrap()).unwrap(),
            serde_json::to_vec(&expected.call_tree(file, symbol, 3).unwrap()).unwrap(),
            "call_tree {file:?} {symbol}"
        );
        assert_eq!(
            format!(
                "{:?}",
                actual
                    .outgoing_calls_of(&actual.node_for(file, symbol).unwrap())
                    .unwrap()
            )
            .as_bytes(),
            format!(
                "{:?}",
                expected
                    .outgoing_calls_of(&expected.node_for(file, symbol).unwrap())
                    .unwrap()
            )
            .as_bytes(),
            "callees {file:?} {symbol}"
        );
    }
}

#[test]
fn blob_reader_patch_selects_one_file_and_reuses_dispatch() {
    let f = fixture();
    let connection = Connection::open(&f.blobs).unwrap();
    let files = (0..12)
        .map(|i| {
            (
                format!("file_{i}.ts"),
                format!("export function f{i}(x) {{ x.run(); }}"),
            )
        })
        .collect::<Vec<_>>();
    let entries = |edited| {
        files
            .iter()
            .map(|(path, source)| {
                (
                    path.clone(),
                    if edited && path == "file_0.ts" {
                        format!("{source}\nexport function added() {{}}")
                    } else {
                        source.clone()
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    let versions = [false, true].map(|edited| {
        let files = entries(edited);
        manifest(
            &connection,
            &files
                .iter()
                .map(|(path, source)| (path.as_str(), source.as_str()))
                .collect::<Vec<_>>(),
        )
    });
    let reader = v3_reader(&connection, &f.dir.path().join("v3"));
    let patched = f.dir.path().join("derived-selected.sqlite");
    let cold = f.dir.path().join("derived-cold.sqlite");
    materialize_from_blob_reader(&patched, &versions[0], &reader).unwrap();
    let stats = apply_manifest_diff_from_blob_reader(&patched, &versions[0], &versions[1], &reader)
        .unwrap();
    assert_eq!(stats.resolved_files, 1, "{stats:?}");
    assert_eq!(stats.dispatch_resolved_files, 1, "{stats:?}");
    assert!(!stats.full_resolution && !stats.dispatch_full, "{stats:?}");
    materialize_from_blob_reader(&cold, &versions[1], &reader).unwrap();
    parity::assert_logical_parity(&patched, &cold);
    assert_query_parity(&patched, &cold, f.dir.path(), &[("file_0.ts", "f0")]);
}

#[test]
fn blob_reader_patch_relinks_incoming_ordinals() {
    let f = relinking_fixture();
    let connection = Connection::open(&f.blobs).unwrap();
    let reader = v3_reader(&connection, &f.dir.path().join("v3"));
    let patched = f.dir.path().join("derived-relinked.sqlite");
    let cold = f.dir.path().join("derived-cold.sqlite");
    materialize_from_blob_reader(&patched, &f.base, &reader).unwrap();
    let before = snapshot(&patched);
    let stats = apply_manifest_diff_from_blob_reader(&patched, &f.base, &f.next, &reader).unwrap();
    assert!(!stats.full_resolution && !stats.dispatch_full, "{stats:?}");
    assert!(stats.relinked_inserted > 0, "{stats:?}");
    materialize_from_blob_reader(&cold, &f.next, &reader).unwrap();
    assert_ne!(before["edges"], snapshot(&cold)["edges"]);
    parity::assert_logical_parity(&patched, &cold);
    assert_query_parity(
        &patched,
        &cold,
        f.dir.path(),
        &[("target.ts", "target"), ("caller.ts", "caller")],
    );
}

#[test]
fn blob_reader_patch_language_corpus_matches_full_through_edits_and_renames() {
    let f = fixture();
    let connection = Connection::open(&f.blobs).unwrap();
    let mut corpus = DifferentialCorpus::new(7).with_directory_packages();
    let mut versions = Vec::new();
    for choice in [
        None,
        Some(0),
        Some(5),
        Some(6),
        Some(8),
        Some(9),
        Some(12),
        Some(14),
        Some(15),
        Some(16),
        Some(17),
        Some(18),
        Some(19),
    ] {
        if let Some(choice) = choice {
            corpus.apply(choice);
        }
        versions.push(manifest(
            &connection,
            &corpus
                .files
                .iter()
                .map(|(path, source)| (path.as_str(), source.as_str()))
                .collect::<Vec<_>>(),
        ));
    }
    let renamed = corpus.files.remove("mod_0.ts").unwrap();
    corpus.files.insert("renamed.ts".into(), renamed);
    versions.push(manifest(
        &connection,
        &corpus
            .files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect::<Vec<_>>(),
    ));
    let reader = v3_reader(&connection, &f.dir.path().join("v3"));
    let patched = f.dir.path().join("derived-chain.sqlite");
    materialize_from_blob_reader(&patched, &versions[0], &reader).unwrap();
    let mut from = 0;
    // Reverse transitions exercise removal of the added declarations and files.
    for to in (1..versions.len()).chain((0..versions.len() - 1).rev()) {
        let stats =
            apply_manifest_diff_from_blob_reader(&patched, &versions[from], &versions[to], &reader)
                .unwrap();
        assert!(
            !stats.full_resolution && !stats.dispatch_full,
            "{from}->{to}: {stats:?}"
        );
        let cold = f
            .dir
            .path()
            .join(format!("derived-cold-{from}-{to}.sqlite"));
        materialize_from_blob_reader(&cold, &versions[to], &reader).unwrap();
        parity::assert_logical_parity(&patched, &cold);
        assert_query_parity(
            &patched,
            &cold,
            f.dir.path(),
            &[
                ("draw.ts", "draw"),
                ("src/lib.rs", "entry"),
                ("gosvc/talk.go", "Talk"),
            ],
        );
        from = to;
    }
}

#[test]
fn blob_reader_patch_all_supported_language_fixtures_match_full() {
    use crate::callgraph_store::position_identity_tests::language_fixtures;
    for (path, language, source) in language_fixtures() {
        let f = fixture();
        let connection = Connection::open(&f.blobs).unwrap();
        let versions = [source.to_string(), format!("\n{source}")].map(|source| {
            let blob = join::CallgraphBlob::extract(&source, language, "fixture").unwrap();
            let payload = blob.to_bytes().unwrap();
            let key = blake3::hash(&payload);
            connection
                .execute(
                    "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
                    params![
                        key.as_bytes().as_slice(),
                        payload,
                        blake3::hash(&payload).as_bytes().as_slice()
                    ],
                )
                .unwrap();
            Manifest::new([(
                RelPath::new(path.as_bytes()).unwrap(),
                ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: RegularPlanes {
                        callgraph: Some(key.to_hex().to_string()),
                        semantic: None,
                    },
                    resolution_input: false,
                },
            )])
            .unwrap()
        });
        let reader = v3_reader(&connection, &f.dir.path().join("v3"));
        let patched = f.dir.path().join("derived-language.sqlite");
        let cold = f.dir.path().join("derived-cold.sqlite");
        materialize_from_blob_reader(&patched, &versions[0], &reader).unwrap();
        let stats =
            apply_manifest_diff_from_blob_reader(&patched, &versions[0], &versions[1], &reader)
                .unwrap();
        assert!(
            !stats.full_resolution && !stats.dispatch_full,
            "{language}: {stats:?}"
        );
        materialize_from_blob_reader(&cold, &versions[1], &reader).unwrap();
        parity::assert_logical_parity(&patched, &cold);
        let blob = join::CallgraphBlob::extract(source, language, "fixture").unwrap();
        let queries = blob
            .parse()
            .unwrap()
            .symbols
            .iter()
            .map(|symbol| (path, symbol.scoped_name.as_str()))
            .collect::<Vec<_>>();
        assert!(!queries.is_empty(), "{language} must exercise queries");
        assert_query_parity(&patched, &cold, f.dir.path(), &queries);
    }
}

#[test]
fn graph_answers_ignore_edge_insertion_order_at_tied_positions() {
    let f = fixture();
    let connection = Connection::open(&f.blobs).unwrap();
    let corpus = DifferentialCorpus::new(7);
    let manifest = manifest(
        &connection,
        &corpus
            .files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect::<Vec<_>>(),
    );
    let reader = v3_reader(&connection, &f.dir.path().join("v3"));
    let forward = f.dir.path().join("derived-forward.sqlite");
    let reverse = f.dir.path().join("derived-reverse.sqlite");
    materialize_from_blob_reader(&forward, &manifest, &reader).unwrap();
    {
        let connection = Connection::open(&forward).unwrap();
        let inserted = connection.execute("INSERT INTO edges SELECT edge_id || ':alternate', ref_id, source_node, target_node, target_file, target_symbol, kind, line, 'exact' FROM edges WHERE provenance='dispatch'", []).unwrap();
        assert!(inserted > 0, "incoming ties must differ in provenance");
    }
    crate::views::generation::clone_derived_for_test(&forward, &reverse).unwrap();
    {
        let connection = Connection::open(&reverse).unwrap();
        connection.execute_batch("CREATE TEMP TABLE reversed_edges AS SELECT * FROM edges; DELETE FROM edges; INSERT INTO edges SELECT * FROM reversed_edges ORDER BY edge_id DESC; DROP TABLE reversed_edges").unwrap();
    }
    parity::assert_logical_parity(&forward, &reverse);
    assert_query_parity(
        &reverse,
        &forward,
        f.dir.path(),
        &[
            ("draw.ts", "draw"),
            ("shapes.ts", "Square::area"),
            ("src/shapes.rs", "Tile::width"),
        ],
    );
}
