//! Opt-in acquisition on a real checkout. Preparation is outside the measured
//! process so legacy builds cannot inflate the views-on RSS high-water mark.

use super::*;

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
        for file in search_index_symbol_files(&index) {
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
