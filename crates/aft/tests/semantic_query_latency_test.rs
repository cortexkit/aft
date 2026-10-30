//! Manual native latency probe. Query and cold-build roots use separate
//! SemanticEmbeddingModel instances, so any shared locking is on process-wide
//! native sessions rather than one instance's mutex.

use aft::config::SemanticBackendConfig;
use aft::semantic_index::{QueryBudget, SemanticEmbeddingModel, SemanticIndex};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires native ORT and cached MiniLM; compares query scheduling with the build reference"]
fn native_query_role_vectors_match_build_baseline() {
    let mut baseline = aft::local_embed::LocalEmbedder::new("all-MiniLM-L6-v2").unwrap();
    let mut query =
        SemanticEmbeddingModel::from_config_for_query(&SemanticBackendConfig::default()).unwrap();
    let corpus = [
        String::new(),
        "where is semantic model memory bounded".into(),
        "pub fn checksum(input: u64) -> u64 { input.wrapping_mul(37) }".into(),
        "Café naïve 日本語 🦀".into(),
        "attention mask pooling ".repeat(600),
    ];
    for text in corpus {
        // The build reference uses batched tokenization and its CPU thread cap;
        // query encoding runs inline with one native thread and a warm arena.
        let expected = baseline.embed(&[text.clone()]).unwrap().remove(0);
        let actual = query.embed(vec![text]).unwrap().remove(0);
        assert_eq!(
            actual.into_iter().map(f32::to_bits).collect::<Vec<_>>(),
            expected.into_iter().map(f32::to_bits).collect::<Vec<_>>(),
            "query scheduling changed embedding bits",
        );
    }
}

#[test]
#[ignore = "bounded manual MiniLM latency measurement; requires native ORT, cached model and probe paths"]
fn native_query_embedding_latency_probe() {
    let root = PathBuf::from(std::env::var_os("AFT_LATENCY_ROOT").expect("AFT_LATENCY_ROOT"));
    let output = PathBuf::from(std::env::var_os("AFT_LATENCY_OUTPUT").expect("AFT_LATENCY_OUTPUT"));
    let label = std::env::var("AFT_LATENCY_LABEL").expect("AFT_LATENCY_LABEL");
    let query_api = std::env::var_os("AFT_LATENCY_QUERY_API").is_some();
    let mut config = SemanticBackendConfig::default();
    config.query_timeout_ms = 500;
    config.route_project_root = Some(root.with_file_name("query-root"));
    let budget_ms = config.query_timeout_ms;
    let started = Instant::now();
    let mut query = SemanticEmbeddingModel::from_config_for_query(&config).unwrap();
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
    query.embed(vec!["warm query model".into()]).unwrap();
    let mut states = Vec::new();
    for state in ["resident_idle", "first_after_idle", "concurrent_cold_build"] {
        let keep_building = Arc::new(AtomicBool::new(true));
        let build_finished = Arc::new(AtomicBool::new(false));
        let mut build = None;
        if state == "concurrent_cold_build" {
            let files = (0..400)
                .map(|file| root.join(format!("file_{file}.rs")))
                .collect::<Vec<_>>();
            let (begun, started) = std::sync::mpsc::sync_channel(1);
            let root = root.clone();
            let mut config = config.clone();
            config.route_project_root = Some(root.clone());
            let active = Arc::clone(&keep_building);
            let finished = Arc::clone(&build_finished);
            build = Some(std::thread::spawn(move || {
                let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
                let mut first = Some(begun);
                let result = SemanticIndex::build_with_progress_and_cancellation(
                    &root,
                    &files,
                    &mut |texts| {
                        if let Some(begun) = first.take() {
                            begun.send(()).unwrap();
                        }
                        model.embed(texts)
                    },
                    64,
                    &mut |_, _| {},
                    &mut || active.load(Ordering::Acquire),
                );
                finished.store(true, Ordering::Release);
                result
            }));
            started.recv_timeout(Duration::from_secs(120)).unwrap();
            // The notification arrives just before the build's first embed
            // call. Give that native call a chance to start before timing queries.
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut samples = Vec::new();
        let mut errors = Vec::new();
        let cache_before = query.query_embedding_cache_stats();
        let mut idle_sessions = Vec::new();
        for number in 0..20 {
            if state == "first_after_idle" {
                // The model reaper checks every 15 s after a 60 s idle TTL.
                // Every sample is an independently cold-first query, not one
                // cold sample hidden among nineteen warm queries.
                std::thread::sleep(Duration::from_secs(76));
                let snapshot = serde_json::to_value(aft::memory::MemorySnapshot::new(
                    "ready",
                    Default::default(),
                ))
                .unwrap();
                let sessions = snapshot["process"]["embedding_sessions"].as_u64();
                if label == "single" {
                    assert_eq!(sessions, Some(0), "idle session did not unload");
                }
                idle_sessions.push(sessions);
            }
            if state == "concurrent_cold_build" {
                assert!(
                    !build_finished.load(Ordering::Acquire),
                    "cold build ended before all query samples"
                );
            }
            let text = format!("where is semantic model memory bounded for query {state} {number}");
            let started = Instant::now();
            // Native-work mode avoids censoring long waits at the deadline.
            // API mode also measures worker handoff and records budget failures.
            let result = if query_api {
                query
                    .embed_query_cached(&text, QueryBudget::from_config(&config))
                    .map(|vector| vec![vector])
            } else {
                query.embed(vec![text])
            };
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            if let Ok(vectors) = &result {
                assert_eq!(vectors.len(), 1);
                assert_eq!(vectors[0].len(), 384);
            }
            errors.push(result.err());
            samples.push(elapsed_ms);
            eprintln!("LATENCY {label} {state} {number} {elapsed_ms:.3}ms");
        }
        keep_building.store(false, Ordering::Release);
        if let Some(build) = build {
            let _ = build.join().expect("cold-build thread panicked");
        }
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        let cache_after = query.query_embedding_cache_stats();
        if query_api {
            assert_eq!(cache_after.0 - cache_before.0, 0, "measured a cache hit");
            assert_eq!(
                cache_after.1 - cache_before.1,
                20,
                "missing query cache misses"
            );
        }
        let row = json!({
            "state": state, "samples_ms": samples, "p50_ms": sorted[9],
            "sessions_before_idle_queries": idle_sessions,
            "p95_ms": sorted[18], "max_ms": sorted[19],
            "budget_ms": budget_ms,
            "over_budget": sorted.iter().filter(|ms| **ms > budget_ms as f64).count(),
            "successes": errors.iter().filter(|error| error.is_none()).count(),
            "errors": errors,
        });
        states.push(row);
        std::fs::write(
            &output,
            serde_json::to_vec_pretty(&json!({
                "label": label, "query_api": query_api, "initial_load_ms": load_ms, "states": states,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    assert!(
        states.iter().all(|row| row["successes"] == 20),
        "query embedding failed; see recorded outcomes"
    );
}
