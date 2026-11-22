pub use aft_base::search_hot_path_measurements::*;
use std::sync::atomic::Ordering;

pub(crate) fn measure<T>(case: &str, operation: impl FnOnce() -> T) -> T {
    FILE_READS.store(0, Ordering::SeqCst);
    reset();
    #[cfg(debug_assertions)]
    crate::search_index::reset_postings_for_trigram_count_for_debug();
    let started = std::time::Instant::now();
    let (result, allocations, allocated_bytes) = crate::test_allocations::count_process(operation);
    record(|counts| counts.file_reads = FILE_READS.load(Ordering::SeqCst));
    #[cfg(debug_assertions)]
    let posting_reads = Some(crate::search_index::postings_for_trigram_count_for_debug());
    #[cfg(not(debug_assertions))]
    let posting_reads = None::<usize>;
    println!(
        "HOT_PATH {}",
        serde_json::json!({
            "case": case,
            "profile": "dev-test",
            "latency_ms": started.elapsed().as_secs_f64() * 1000.0,
            "allocations": allocations,
            "allocated_bytes": allocated_bytes,
            "posting_reads": posting_reads,
            "work": counts(),
        })
    );
    result
}

pub(crate) fn result_digest(case: &str, value: &impl serde::Serialize) {
    let mut value = serde_json::to_value(value).unwrap();
    fn relative_paths(value: &mut serde_json::Value, root: &str) {
        match value {
            serde_json::Value::String(text) => {
                if let Some(relative) = text
                    .strip_prefix(root)
                    .filter(|relative| relative.starts_with(std::path::MAIN_SEPARATOR))
                {
                    *text = format!("<corpus>{relative}");
                }
            }
            serde_json::Value::Array(values) => values
                .iter_mut()
                .for_each(|value| relative_paths(value, root)),
            serde_json::Value::Object(values) => values
                .values_mut()
                .for_each(|value| relative_paths(value, root)),
            _ => {}
        }
    }
    if let Ok(root) = std::env::var("AFT_PERF_CORPUS") {
        relative_paths(&mut value, &root);
    }
    let bytes = serde_json::to_vec(&value).unwrap();
    println!(
        "HOT_PATH {}",
        serde_json::json!({"case": case, "ranked_blake3": blake3::hash(&bytes).to_hex().to_string()})
    );
}

#[test]
#[ignore = "set AFT_PERF_CORPUS to a real corpus; opt-in disk and allocation benchmark"]
fn real_corpus_lane_work_counts() {
    use crate::commands::semantic_search::{
        anchored_lane::AnchoredLane, lexical_lane::CanonicalLexicalLane,
    };
    use crate::search_index::{extract_trigrams, SearchIndex};
    let root =
        std::path::PathBuf::from(std::env::var_os("AFT_PERF_CORPUS").expect("AFT_PERF_CORPUS"));
    let mut index = SearchIndex::new();
    let mut files = 0;
    for entry in ignore::WalkBuilder::new(&root)
        .hidden(true)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry
            .metadata()
            .is_ok_and(|metadata| metadata.len() <= 1024 * 1024)
        {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                index.index_file(entry.path(), &bytes);
                files += 1;
            }
        }
    }
    assert!(files > 0, "corpus must contain indexed files");
    index.ready = true;
    let snapshot = index.snapshot();
    for (label, query) in [
        ("natural_language", "how does the search index discover candidate files and verify exact matches before ranking results"),
        ("code_literal", "SearchIndex::new"),
        ("common_rare", "search repeated_page_marker"),
    ] {
        let trigrams = extract_trigrams(query.as_bytes()).into_iter().map(|(trigram, _, _)| trigram).collect::<Vec<_>>();
        let lane = measure(&format!("lexical/{label}"), || CanonicalLexicalLane::from_snapshot(&snapshot, &trigrams, None, 50).unwrap());
        result_digest(&format!("lexical/{label}"), &lane.canonical_order());
        let exact = measure(&format!("exact/{label}"), || snapshot.whole_corpus_exact_pass(query, &root, None).unwrap());
        result_digest(&format!("exact/{label}"), &exact.iter().map(|hit| (&hit.path, hit.symbol_range, &hit.evidence, &hit.content_digest)).collect::<Vec<_>>());
    }
    let anchored = measure("anchored/search.*index", || {
        AnchoredLane::new()
            .execute_ready_mode(&index, &root, "search.*index", true)
            .unwrap()
    });
    result_digest("anchored/search.*index", &anchored);
}
