//! Opt-in acquisition on a real checkout. Preparation is outside the measured
//! process so legacy builds cannot inflate the views-on RSS high-water mark.

use super::*;

const KEY: &str = "0011223344556677";
const LIMIT: u64 = 1_048_576;

fn warm_fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let checkout = tempfile::tempdir().unwrap();
    let root = checkout.path().canonicalize().unwrap();
    std::fs::write(root.join("lib.rs"), "pub fn alpha() { beta(); }\nfn beta() {}\n").unwrap();
    std::fs::write(root.join("other.rs"), "pub fn gamma() { }\n").unwrap();
    let storage = tempfile::tempdir().unwrap();
    let index = SearchIndex::build_with_limit_to_cache_dir(&root, LIMIT, &storage.path().join("index").join(KEY));
    let mut parser = crate::parser::FileParser::new();
    let mut cache = crate::parser::SymbolCache::new();
    cache.set_project_root(root.clone());
    for entry in search_index_symbol_files(&index).iter() {
        let symbols = parser.extract_symbols(&entry.path).unwrap();
        cache.insert(entry.path.clone(), entry.modified, entry.size, entry.content_hash, symbols);
    }
    crate::symbol_cache_disk::write_to_disk(&cache, storage.path(), KEY).unwrap();
    (checkout, storage, root)
}

fn lexical_snapshot(root: &Path, storage: &Path) -> crate::search_index::SearchIndexSnapshot {
    let view = crate::views::trigram::LexicalView::new(
        root.to_path_buf(), crate::root_cache::StoragePaths::same(storage), KEY.into(),
        crate::blob_store::v2::TrigramPolicy { max_file_size: LIMIT },
    ).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match view.snapshot() {
            Ok(snapshot) => return snapshot,
            Err(error) if crate::views::trigram::bind_is_busy(&error) && Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!("{error}"),
        }
    }
}

#[test]
fn views_import_warm_trigram_donor_avoids_full_extraction() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let (_checkout, storage, root) = warm_fixture();
    use crate::views::segment_store::TrigramPayload;
    let before = TrigramPayload::extraction_count();
    let _snapshot = lexical_snapshot(&root, storage.path());
    assert_eq!(TrigramPayload::extraction_count() - before, 0, "warm donor must bypass trigram extraction");
    let _cold = TrigramPayload::extract(b"counter control", &crate::blob_store::v2::TrigramPolicy { max_file_size: LIMIT });
    assert!(TrigramPayload::extraction_count() > before, "counter must observe real work");
}

#[test]
fn views_import_warm_symbol_donor_avoids_full_extraction() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let (_checkout, storage, root) = warm_fixture();
    let _snapshot = lexical_snapshot(&root, storage.path());
    let index = SearchIndex::read_from_disk(&storage.path().join("index").join(KEY), &root).unwrap();
    let cache = Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new()));
    let generation = cache.read().unwrap().generation();
    let before = crate::parser::work_counters::cached_parser_parses();
    let report = prewarm_symbol_cache_from_view(storage.path(), KEY, &cache, generation, &search_index_symbol_files(&index));
    assert_eq!(report.reused, 2);
    assert_eq!(report.stored, 0);
    assert_eq!(crate::parser::work_counters::cached_parser_parses() - before, 0);
}

fn benchmark_paths() -> (PathBuf, PathBuf, String) {
    let root = PathBuf::from(std::env::var_os("AFT_IMPORT_BENCH_ROOT").expect("benchmark root"))
        .canonicalize()
        .unwrap();
    let storage = PathBuf::from(std::env::var_os("AFT_IMPORT_BENCH_STORAGE").expect("benchmark storage"));
    let plane = std::env::var("AFT_IMPORT_BENCH_PLANE").expect("benchmark plane");
    (root, storage, plane)
}

#[test]
#[ignore = "manual large-checkout acquisition; requires isolated benchmark paths"]
fn views_import_benchmark_prepare() {
    let (root, storage, plane) = benchmark_paths();
    let key = "0011223344556677";
    std::fs::create_dir_all(&storage).unwrap();
    let index = SearchIndex::build_with_limit_to_cache_dir(
        &root,
        1_048_576,
        &storage.join("index").join(key),
    );
    println!("corpus={} files={}", root.display(), index.file_count());
    if plane == "symbols" {
        let cache = Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new()));
        let generation = cache.read().unwrap().generation();
        cache.write().unwrap().set_project_root_for_generation(generation, root.clone());
        let mut parser = crate::parser::FileParser::with_symbol_cache(cache.clone());
        for file in search_index_symbol_files(&index).iter() {
            let _ = prewarm_symbol_cache_file(&mut parser, &file.path);
        }
        crate::symbol_cache_disk::write_to_disk(&cache.read().unwrap(), &storage, key)
            .unwrap();
    }
    if plane == "callgraph" {
        let (store, stats) = crate::callgraph_store::CallGraphStore::cold_build_with_lease_chunked(
            storage.join("callgraph").join(key), root, &[], 256,
        ).unwrap();
        println!("legacy_graph_files={}", stats.files);
        drop(store);
    }
}

#[test]
#[ignore = "manual large-checkout acquisition; requires prepared legacy artifacts"]
fn views_import_benchmark_measure() {
    let (root, storage, plane) = benchmark_paths();
    let key = "0011223344556677";
    let started = Instant::now();
    let policy = crate::blob_store::v2::TrigramPolicy { max_file_size: 1_048_576 };
    let paths = crate::root_cache::StoragePaths::same(&storage);
    match plane.as_str() {
        "trigram" => {
            let view = crate::views::trigram::LexicalView::new(root, paths, key.into(), policy).unwrap();
            let _snapshot = view.snapshot().unwrap();
        }
        "symbols" => {
            let index = SearchIndex::read_from_disk(&storage.join("index").join(key), &root).unwrap();
            let cache = Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new()));
            let generation = cache.read().unwrap().generation();
            let report = prewarm_symbol_cache_from_view(&storage, key, &cache, generation, &search_index_symbol_files(&index));
            println!("symbols_reused={} symbols_extracted={}", report.reused, report.stored);
        }
        "callgraph" => {
            let view = crate::views::callgraph::CallgraphView::new(root, paths, key.into(), policy).unwrap();
            let _reader = view.reader().unwrap();
            println!("graph_extractions={}", view.extraction_count());
        }
        _ => panic!("unknown plane"),
    }
    println!("plane={plane} wall_seconds={:.6}", started.elapsed().as_secs_f64());
}
