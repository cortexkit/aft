//! Measures meaning search on the data layer over a real repository with
//! the configured embedding model (by default the local MiniLM): binds a
//! checkout to a fresh store, builds it, fills its vectors, fills again
//! (which must embed nothing), searches it, and then fills a second
//! checkout of the same repository at another commit, which must embed
//! only what the first lacked.
//!
//! ```text
//! cargo run --profile stage -p agent-file-tools --example meaning_fill -- \
//!     <checkout> <second checkout> <empty store dir> [query file]
//! ```
//!
//! The query file holds one query per line; without one, a few fixed
//! queries about AFT are used. Prints one JSON object to stdout and
//! progress to stderr.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aft::config::SemanticBackendConfig;
use aft::data_layer::checkout::{Checkout, DataLayer};
use aft::data_layer::vectors::{FillBudget, FillReport, Producer, VectorPlane};
use aft::semantic_index::SemanticEmbeddingModel;
use aft_store::cache::BlockCache;
use aft_store::family::Family;
use aft_store::ids::{Digest128, FamilyId};
use aft_store::segment::KnownSection;
use aft_store::vector::{SectionBytes, VectorSegment};
use serde_json::json;

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn percentile(sorted: &[f64], share: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let at = ((sorted.len() - 1) as f64 * share).round() as usize;
    sorted[at]
}

/// Fills `plane`, logging each round.
fn fill(
    label: &str,
    plane: &VectorPlane,
    model: &mut SemanticEmbeddingModel,
    budget: FillBudget,
) -> (FillReport, f64) {
    let started = Instant::now();
    let report = plane
        .fill(&mut |texts| model.embed(texts), budget, &mut |step| {
            eprintln!(
                "{label}: {}/{} files, {} chunks embedded, {:.0} s",
                step.files_done,
                step.files_total,
                step.chunks_embedded,
                started.elapsed().as_secs_f64()
            );
        })
        .unwrap();
    (report, ms(started))
}

/// The vector sections' bytes in the family's newest generation, and how
/// many segments hold them.
fn vector_bytes(store: &Path, family: FamilyId) -> (SectionBytes, usize, u64) {
    let family = Family::open(store, family).unwrap();
    let snapshot = family.snapshot().unwrap();
    let known: Vec<KnownSection> = aft_store::text::KNOWN
        .into_iter()
        .chain(aft_store::content::KNOWN)
        .chain(aft_store::graph::KNOWN)
        .chain(aft_store::vector::KNOWN)
        .collect();
    let cache = Arc::new(BlockCache::new(64 << 20));
    let mut total = SectionBytes::default();
    let mut segments = 0;
    let mut all = 0;
    for entry in snapshot.manifest().segments() {
        all += entry.len;
        let segment = snapshot
            .open_segment(entry.id, &known, Arc::clone(&cache))
            .unwrap();
        if let Some(vectors) = VectorSegment::open(&segment).unwrap() {
            let bytes = vectors.section_bytes();
            segments += 1;
            total.units += bytes.units;
            total.scan += bytes.scan;
            total.vectors += bytes.vectors;
            total.chunk_maps += bytes.chunk_maps;
            total.chunk_details += bytes.chunk_details;
        }
    }
    (total, segments, all)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first_root = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let second_root = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let store = PathBuf::from(args.next().unwrap());
    let queries: Vec<String> = match args.next() {
        Some(path) => std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        None => [
            "where is the grep candidate planner",
            "how does a checkout publish its state",
            "embedding backend batch size",
            "call graph callers of a symbol",
            "lease file reclamation horizon",
            "parse a rust file into symbols",
            "format the search results footer",
            "watcher events for changed paths",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
    };
    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"meaning fill measurement"));

    let started = Instant::now();
    let first = Arc::new(Checkout::bind(Arc::clone(&layer), &store, family, &first_root).unwrap());
    let first_build = first.build().unwrap();
    let first_build_ms = ms(started);
    eprintln!("built {} in {first_build_ms:.0} ms", first_root.display());

    let config = SemanticBackendConfig::default();
    let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
    let fingerprint = model.fingerprint(&config).unwrap();
    let producer = Producer::from_fingerprint(&fingerprint);
    let budget = FillBudget {
        max_batch: model.max_batch_size(),
        files_per_round: 256,
    };
    let plane = VectorPlane::new(Arc::clone(&first), producer.clone(), config.max_files);
    let (first_fill, first_fill_ms) = fill("first", &plane, &mut model, budget);
    let (again, again_ms) = fill("again", &plane, &mut model, budget);
    assert_eq!(again.chunks_embedded, 0, "a second fill embeds nothing");

    let view = first.view().unwrap();
    let started = Instant::now();
    let coverage = plane.coverage(&view).unwrap();
    let coverage_ms = ms(started);
    let mut latencies = Vec::new();
    let mut hits = 0;
    for query in &queries {
        let vector = model.embed(vec![query.clone()]).unwrap().remove(0);
        // The first search of a query warms nothing the next one needs; a
        // few runs show the steady cost.
        for _ in 0..5 {
            let started = Instant::now();
            let answer = plane.search(&view, &vector, 51, &|_| true).unwrap();
            latencies.push(ms(started));
            hits = answer.results.len();
        }
    }
    latencies.sort_by(f64::total_cmp);
    let (bytes, vector_segments, store_bytes) = vector_bytes(&store, family);

    let started = Instant::now();
    let second =
        Arc::new(Checkout::bind(Arc::clone(&layer), &store, family, &second_root).unwrap());
    second.build().unwrap();
    let second_build_ms = ms(started);
    let second_plane = VectorPlane::new(Arc::clone(&second), producer, config.max_files);
    let (second_fill, second_fill_ms) = fill("second", &second_plane, &mut model, budget);
    let second_coverage = second_plane.coverage(&second.view().unwrap()).unwrap();

    let summary = json!({
        "model": fingerprint.model,
        "dimensions": fingerprint.dimension,
        "first": {
            "root": first_root,
            "build": first_build,
            "build_ms": first_build_ms,
            "fill": first_fill,
            "fill_ms": first_fill_ms,
            "fill_again": again,
            "fill_again_ms": again_ms,
            "coverage": {
                "files": coverage.files,
                "files_embedded": coverage.files_embedded,
                "files_too_large": coverage.files_too_large,
                "files_failed": coverage.files_failed.len(),
                "failed_examples": coverage.files_failed.iter().take(5).collect::<Vec<_>>(),
                "pending": coverage.pending.len(),
                "past_cap": coverage.past_cap,
                "chunks": coverage.chunks,
                "chunks_embedded": coverage.chunks_embedded,
                "chunks_failed": coverage.chunks_failed.len(),
                "complete": coverage.complete(),
            },
            "coverage_ms": coverage_ms,
            "search_ms": {
                "queries": queries.len(),
                "runs": latencies.len(),
                "k": 51,
                "hits": hits,
                "p50": percentile(&latencies, 0.5),
                "p95": percentile(&latencies, 0.95),
                "max": percentile(&latencies, 1.0),
            },
        },
        "store": {
            "vector_segments": vector_segments,
            "units": bytes.units,
            "scan": bytes.scan,
            "f32": bytes.vectors,
            "chunk_maps": bytes.chunk_maps,
            "chunk_details": bytes.chunk_details,
            "all_segments": store_bytes,
        },
        "second": {
            "root": second_root,
            "build_ms": second_build_ms,
            "fill": second_fill,
            "fill_ms": second_fill_ms,
            "coverage_complete": second_coverage.complete(),
            "files_embedded": second_coverage.files_embedded,
            "files": second_coverage.files,
        },
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}
