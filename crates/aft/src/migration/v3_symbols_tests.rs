use std::path::Path;

use super::*;
use crate::blob_store::v2::FamilyPlane;
use crate::blob_store::v3;
use crate::migration::per_checkout::ImportState;
use crate::migration::v3_import::tests::{checkout, snapshot_tree, FAMILY, KEY, POLICY};
use crate::migration::v3_import::LegacyArtifact;
use crate::migration::v3_ledger::{read_rows, run_v3_import, ArtifactOutcome, V3ImportRequest};
use crate::parser::work_counters::cached_parser_parses;

/// Parses every supported file under `root` (this is the cold work an import
/// must avoid) and writes the previous release's `symbols.bin` for it.
/// Returns each relative path's bytes and symbols.
fn legacy_symbols(storage: &Path, root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>, Vec<Symbol>)> {
    let mut parser = crate::parser::FileParser::new();
    let mut cache = crate::parser::SymbolCache::new();
    cache.set_project_root(root.to_path_buf());
    let mut files = Vec::new();
    for (path, bytes) in snapshot_tree(root) {
        if crate::parser::detect_language(&path).is_none() {
            continue;
        }
        let symbols = parser.extract_symbols(&path).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        cache.insert(
            path.clone(),
            metadata.modified().unwrap(),
            metadata.len(),
            blake3::hash(&bytes),
            symbols.clone(),
        );
        files.push((
            path.strip_prefix(root).unwrap().to_path_buf(),
            bytes,
            symbols,
        ));
    }
    crate::symbol_cache_disk::write_to_disk(&cache, storage, KEY).unwrap();
    assert!(crate::symbol_cache_disk::cache_path(storage, KEY).is_file());
    files
}

fn request(storage: &Path, root: &Path) -> V3ImportRequest {
    V3ImportRequest::full_plane(
        storage.to_path_buf(),
        FAMILY.to_owned(),
        KEY.to_owned(),
        root.to_path_buf(),
        POLICY,
        None,
    )
}

/// A compatible symbol cache is imported and then served by key with zero
/// tree-sitter parses; the counter is shown to count real parses.
#[test]
fn a_compatible_symbol_import_needs_no_parse_and_serves_lookups() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    let files = legacy_symbols(&storage, &root);
    // Every checkout file the parser has a language for, and only those.
    assert_eq!(
        files
            .iter()
            .map(|(relative, _, _)| relative.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        ["notes/readme.md", "src/alpha.rs", "src/beta.rs"]
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect()
    );
    let legacy = snapshot_tree(&storage);

    let parses = cached_parser_parses();
    let report = run_v3_import(&request(&storage, &root), None).unwrap();
    let ArtifactOutcome::Done { registered, .. } = &report.outcomes[&LegacyArtifact::Symbols]
    else {
        panic!("{:?}", report.outcomes);
    };
    assert_eq!(registered.inserted as usize, files.len());
    for (relative, bytes, symbols) in &files {
        let stored = symbol_store::lookup(&storage, FAMILY, ContentHash::of(bytes), relative)
            .unwrap()
            .unwrap_or_else(|| panic!("no row for {}", relative.display()));
        assert_eq!(&*stored, symbols, "{}", relative.display());
    }
    assert_eq!(
        cached_parser_parses() - parses,
        0,
        "import and lookups must not parse"
    );
    // Control: the counter does count a real parse on this thread.
    crate::parser::FileParser::new()
        .extract_symbols(&root.join(&files[0].0))
        .unwrap();
    assert!(cached_parser_parses() > parses);

    let rows = read_rows(&storage, FAMILY).unwrap().unwrap();
    let row = rows
        .iter()
        .find(|row| row.key.artifact == LegacyArtifact::Symbols)
        .unwrap();
    assert_eq!(row.state, ImportState::Done);
    assert_eq!(row.key.plane, FamilyPlane::Symbol);
    assert_eq!(row.key.producer, SymbolProducer::current().id());
    let after = snapshot_tree(&storage);
    for (path, bytes) in &legacy {
        assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
    }
}

/// A cache written under another extraction schema is rejected whole, and
/// a key under another producer never matches a stored row: a parser or
/// schema change misses.
#[test]
fn a_parser_or_schema_change_misses() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    let files = legacy_symbols(&storage, &root);
    let path = crate::symbol_cache_disk::cache_path(&storage, KEY);
    let mut bytes = fs::read(&path).unwrap();
    let schema = crate::symbol_cache_disk::SCHEMA_VERSION;
    bytes[12..16].copy_from_slice(&(schema - 1).to_le_bytes());
    fs::write(&path, &bytes).unwrap();
    let report = run_v3_import(&request(&storage, &root), None).unwrap();
    let ArtifactOutcome::Rejected(reason) = &report.outcomes[&LegacyArtifact::Symbols] else {
        panic!("{:?}", report.outcomes);
    };
    assert!(reason.contains("schema"), "{reason}");
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "the legacy cache is left as it was"
    );
    for (relative, source, _) in &files {
        assert!(
            symbol_store::lookup(&storage, FAMILY, ContentHash::of(source), relative)
                .unwrap()
                .is_none()
        );
    }

    // A compatible cache imports, but a lookup key of another producer still
    // misses every row.
    let other = tempfile::tempdir().unwrap();
    let storage = other.path().join("storage");
    let root = checkout(other.path());
    let files = legacy_symbols(&storage, &root);
    run_v3_import(&request(&storage, &root), None).unwrap();
    let reader = v3::PlaneReader::open_existing(&storage, FAMILY, FamilyPlane::Symbol).unwrap();
    for (relative, source, _) in &files {
        let language = symbol_store::language_of(relative).unwrap();
        let current = SymbolKey {
            content: ContentHash::of(source),
            language: language.clone(),
            producer: SymbolProducer::current(),
        };
        let changed = SymbolKey {
            producer: SymbolProducer {
                extraction_schema: schema + 1,
            },
            ..current.clone()
        };
        assert!(reader.contains(&current.family_key()).unwrap());
        assert!(!reader.contains(&changed.family_key()).unwrap());
    }
}

/// An entry whose file changed since it was parsed cannot vouch for the new
/// bytes and is left for the plane; the others are imported.
#[test]
fn a_changed_source_is_left_for_the_plane() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    let files = legacy_symbols(&storage, &root);
    let changed = b"pub fn alpha_total() -> u32 { 7 }\n";
    fs::write(root.join("src/alpha.rs"), changed).unwrap();
    let report = run_v3_import(&request(&storage, &root), None).unwrap();
    let ArtifactOutcome::Done {
        registered,
        skipped,
    } = &report.outcomes[&LegacyArtifact::Symbols]
    else {
        panic!("{:?}", report.outcomes);
    };
    assert_eq!(*skipped, 1);
    assert_eq!(registered.inserted as usize, files.len() - 1);
    for (relative, source, _) in &files {
        let found = symbol_store::lookup(&storage, FAMILY, ContentHash::of(source), relative)
            .unwrap()
            .is_some();
        assert_eq!(
            found,
            relative != Path::new("src/alpha.rs"),
            "{}",
            relative.display()
        );
    }
    assert!(symbol_store::lookup(
        &storage,
        FAMILY,
        ContentHash::of(changed),
        Path::new("src/alpha.rs")
    )
    .unwrap()
    .is_none());
}
