//! Opt-in acquisition on a real checkout. Building the pre-view caches happens
//! in a separate preparation process so its memory peak is not charged to the
//! measured views-on acquisition.

use super::*;

const KEY: &str = "0011223344556677";
const LIMIT: u64 = 1_048_576;

fn warm_fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let checkout = tempfile::tempdir().unwrap();
    let root = checkout.path().canonicalize().unwrap();
    std::fs::write(
        root.join("lib.rs"),
        "pub fn alpha() { beta(); }\nfn beta() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("other.rs"), "pub fn gamma() { }\n").unwrap();
    let storage = tempfile::tempdir().unwrap();
    let index = SearchIndex::build_with_limit_to_cache_dir(
        &root,
        LIMIT,
        &storage.path().join("index").join(KEY),
    );
    let mut parser = crate::parser::FileParser::new();
    let mut cache = crate::parser::SymbolCache::new();
    cache.set_project_root(root.clone());
    for entry in search_index_symbol_files(&index).iter() {
        let symbols = parser.extract_symbols(&entry.path).unwrap();
        cache.insert(
            entry.path.clone(),
            entry.modified,
            entry.size,
            entry.content_hash,
            symbols,
        );
    }
    crate::symbol_cache_disk::write_to_disk(&cache, storage.path(), KEY).unwrap();
    (checkout, storage, root)
}

fn lexical_snapshot(root: &Path, storage: &Path) -> crate::search_index::SearchIndexSnapshot {
    let view = crate::views::trigram::LexicalView::new(
        root.to_path_buf(),
        crate::root_cache::StoragePaths::same(storage),
        KEY.into(),
        crate::blob_store::v2::TrigramPolicy {
            max_file_size: LIMIT,
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match view.snapshot() {
            Ok(snapshot) => return snapshot,
            Err(error)
                if crate::views::trigram::bind_is_busy(&error) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10))
            }
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
    assert_eq!(
        TrigramPayload::extraction_count() - before,
        0,
        "warm donor must bypass trigram extraction"
    );
    let _cold = TrigramPayload::extract(
        b"counter control",
        &crate::blob_store::v2::TrigramPolicy {
            max_file_size: LIMIT,
        },
    );
    assert!(
        TrigramPayload::extraction_count() > before,
        "counter must observe real work"
    );
}

#[test]
fn views_import_warm_symbol_donor_avoids_full_extraction() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let (_checkout, storage, root) = warm_fixture();
    let _snapshot = lexical_snapshot(&root, storage.path());
    let index =
        SearchIndex::read_from_disk(&storage.path().join("index").join(KEY), &root).unwrap();
    let cache = Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new()));
    let generation = cache.read().unwrap().generation();
    let before = crate::parser::work_counters::cached_parser_parses();
    let report = prewarm_symbol_cache_from_view(
        storage.path(),
        KEY,
        &cache,
        generation,
        &search_index_symbol_files(&index),
    );
    assert_eq!(report.reused, 2);
    assert_eq!(report.stored, 0);
    assert_eq!(
        crate::parser::work_counters::cached_parser_parses() - before,
        0
    );
    let mut cold = crate::parser::FileParser::new();
    for entry in search_index_symbol_files(&index).iter() {
        let expected = cold.extract_symbols(&entry.path).unwrap();
        let stored = crate::views::symbol_store::lookup(
            storage.path(),
            KEY,
            crate::blob_store::v2::ContentHash::of(&std::fs::read(&entry.path).unwrap()),
            &entry.path,
        )
        .unwrap()
        .unwrap();
        assert_eq!(*stored, expected, "{}", entry.path.display());
    }
}

#[test]
fn views_import_borrow_only_and_unbound_roots_do_not_start_imports() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let (_checkout, storage, root) = warm_fixture();
    let policy = crate::blob_store::v2::TrigramPolicy {
        max_file_size: LIMIT,
    };
    crate::root_cache::configure_artifact_access(&root, KEY, true);
    assert!(crate::migration::views_background::BackgroundImport::start(
        storage.path(),
        KEY,
        &root,
        policy,
        Arc::new(|| true),
        crate::cold_build_limiter::global_limiter(),
    )
    .is_none());
    let request = crate::migration::v3_ledger::V3ImportRequest::full_plane(
        storage.path().to_path_buf(),
        KEY.into(),
        KEY.into(),
        root.clone(),
        policy,
        None,
    );
    assert!(matches!(
        crate::migration::v3_ledger::run_v3_import(&request, None),
        Err(crate::migration::v3_import::V3ImportError::Store(
            crate::blob_store::v3::Error::Refused("legacy_import_borrow_only")
        ))
    ));
    crate::root_cache::configure_artifact_access(&root, KEY, false);
    assert!(crate::migration::views_background::BackgroundImport::start(
        storage.path(),
        KEY,
        &root,
        policy,
        Arc::new(|| false),
        crate::cold_build_limiter::global_limiter(),
    )
    .is_none());
    assert!(!storage.path().join("blobs/v3").exists());
    assert!(!storage.path().join("migration/v3").exists());
}

fn query_context(root: &Path, storage: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.storage_dir = Some(storage.to_path_buf());
    config.views.enabled = true;
    config.indexes.trigram = true;
    config.indexes.semantic = false;
    config.indexes.callgraph = false;
    let context = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    context.set_canonical_cache_root(root.to_path_buf());
    context
}

#[test]
fn views_import_currency_is_revoked_on_unbind_and_not_revived_by_rebind() {
    let (_checkout, storage, root) = warm_fixture();
    let context = query_context(&root, storage.path());
    context.mark_subc_bound();
    let current = context.views_import_control();
    assert!(current());
    context.mark_subc_unbound();
    assert!(!current());
    context.mark_subc_bound();
    assert!(!current(), "a new binding must not revive an old import");
    assert!(context.views_import_control()());
}

#[test]
fn views_import_public_grep_search_outline_zoom_match_fresh_builds() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let (_checkout, storage, root) = warm_fixture();
    let fresh_storage = tempfile::tempdir().unwrap();
    let imported = query_context(&root, storage.path());
    let fresh = query_context(&root, fresh_storage.path());
    let family = imported.memoized_artifact_cache_key(&root);
    crate::root_cache::configure_artifact_access(&root, &family, false);
    for (dir, name) in [("index", "cache.bin"), ("symbols", "symbols.bin")] {
        let destination = storage.path().join(dir).join(&family);
        std::fs::create_dir_all(&destination).unwrap();
        if family != KEY {
            std::fs::copy(
                storage.path().join(dir).join(KEY).join(name),
                destination.join(name),
            )
            .unwrap();
        }
    }
    for ctx in [&imported, &fresh] {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match ctx.lexical_view_snapshot("bind").expect("views must route") {
                Ok(snapshot) => {
                    spawn_symbol_view_prewarm(ctx, &snapshot);
                    break;
                }
                Err(error)
                    if crate::views::trigram::bind_is_busy(&error) && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("{error}"),
            }
        }
        while ctx.symbol_view_route_count() == 0 {
            assert!(Instant::now() < deadline, "symbol prewarm did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }
    type Handler = fn(&crate::protocol::RawRequest, &AppContext) -> crate::protocol::Response;
    let requests: [(Handler, &str, serde_json::Value); 4] = [
        (
            crate::commands::grep::handle_grep,
            "grep",
            serde_json::json!({"pattern":"alpha|beta", "include":"**/*.rs", "max_results":10}),
        ),
        (
            crate::commands::semantic_search::handle_semantic_search,
            "semantic_search",
            serde_json::json!({"query":"alpha"}),
        ),
        (
            crate::commands::outline::handle_outline,
            "outline",
            serde_json::json!({"file":root.join("lib.rs")}),
        ),
        (
            crate::commands::zoom::handle_zoom,
            "zoom",
            serde_json::json!({"file":root.join("lib.rs"), "symbol":"alpha", "context_lines":3}),
        ),
    ];
    for (handler, command, params) in requests {
        let request = crate::protocol::RawRequest {
            id: command.into(),
            command: command.into(),
            lsp_hints: None,
            session_id: None,
            params,
        };
        let warm = handler(&request, &imported);
        let cold = handler(&request, &fresh);
        assert!(warm.success, "{command}: {:?}", warm.data);
        assert!(cold.success, "{command}: {:?}", cold.data);
        for field in ["text", "matches", "results", "symbols", "content", "range"] {
            assert_eq!(warm.data[field], cold.data[field], "{command}.{field}");
        }
        assert!(
            warm.data.to_string().contains("alpha"),
            "{command} must answer the witness, not an empty gap"
        );
    }
    assert!(imported
        .lexical_view_route_counts()
        .iter()
        .any(|count| *count > 0));
    assert!(
        imported.symbol_view_route_count() > 0,
        "outline/zoom must use the v3 symbol route"
    );
}

fn benchmark_paths() -> (PathBuf, PathBuf, String) {
    let root = PathBuf::from(std::env::var_os("AFT_IMPORT_BENCH_ROOT").expect("benchmark root"))
        .canonicalize()
        .unwrap();
    let storage =
        PathBuf::from(std::env::var_os("AFT_IMPORT_BENCH_STORAGE").expect("benchmark storage"));
    let plane = std::env::var("AFT_IMPORT_BENCH_PLANE").expect("benchmark plane");
    crate::root_cache::configure_artifact_access(&root, KEY, false);
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
        cache
            .write()
            .unwrap()
            .set_project_root_for_generation(generation, root.clone());
        let mut parser = crate::parser::FileParser::with_symbol_cache(cache.clone());
        for file in search_index_symbol_files(&index).iter() {
            let _ = prewarm_symbol_cache_file(&mut parser, &file.path);
        }
        crate::symbol_cache_disk::write_to_disk(&cache.read().unwrap(), &storage, key).unwrap();
    }
    if plane == "callgraph" {
        let (store, stats) = crate::callgraph_store::CallGraphStore::cold_build_with_lease_chunked(
            storage.join("callgraph").join(key),
            root,
            &[],
            256,
        )
        .unwrap();
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
    let policy = crate::blob_store::v2::TrigramPolicy {
        max_file_size: 1_048_576,
    };
    let paths = crate::root_cache::StoragePaths::same(&storage);
    match plane.as_str() {
        "trigram" => {
            let view =
                crate::views::trigram::LexicalView::new(root, paths, key.into(), policy).unwrap();
            loop {
                match view.snapshot() {
                    Ok(_snapshot) => break,
                    Err(error) if crate::views::trigram::bind_is_busy(&error) => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        }
        "symbols" => {
            let index =
                SearchIndex::read_from_disk(&storage.join("index").join(key), &root).unwrap();
            // Isolate symbol-plane acquisition; the text donor is timed by the
            // trigram arm rather than charged again to symbol import.
            std::fs::rename(
                storage.join("index").join(key).join("cache.bin"),
                storage.join("text-donor.bin"),
            )
            .unwrap();
            if let Some(job) = crate::migration::views_background::BackgroundImport::start(
                &storage,
                key,
                &root,
                policy,
                Arc::new(|| true),
                crate::cold_build_limiter::global_limiter(),
            ) {
                while job.is_building() {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            let cache = Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new()));
            let generation = cache.read().unwrap().generation();
            let report = prewarm_symbol_cache_from_view(
                &storage,
                key,
                &cache,
                generation,
                &search_index_symbol_files(&index),
            );
            println!(
                "symbols_reused={} symbols_extracted={}",
                report.reused, report.stored
            );
        }
        "callgraph" => {
            let view = crate::views::callgraph::CallgraphView::new(root, paths, key.into(), policy)
                .unwrap();
            loop {
                match view.reader() {
                    Ok(_reader) => break,
                    Err(error) if error.busy => thread::sleep(Duration::from_millis(10)),
                    Err(error) => panic!("{error}"),
                }
            }
            println!("graph_extractions={}", view.extraction_count());
        }
        _ => panic!("unknown plane"),
    }
    println!(
        "plane={plane} wall_seconds={:.6}",
        started.elapsed().as_secs_f64()
    );
}
