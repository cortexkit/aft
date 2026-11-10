//! Text-plane gate on a real checkout and real grep patterns.
//!
//! Scans a checkout into a fresh family, publishes it, binds it back, then
//! for every pattern in a grep corpus checks that each file the regex
//! matches is a candidate (zero misses), and reports candidate counts,
//! query latency, sizes and scan times as one JSON object.
//!
//! ```text
//! cargo run --release -p aft-store --example text_gate -- \
//!     <checkout root> <empty store dir> <patterns.jsonl.gz> [max patterns]
//! ```

#![allow(
    clippy::unwrap_used,
    reason = "a measurement tool: any failure should stop it loudly"
)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "percentile arithmetic over counts far below 2^52"
)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "it reports to the terminal"
)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aft_store::cache::BlockCache;
use aft_store::checkout::{CheckoutState, PathKind};
use aft_store::family::{Family, Publication};
use aft_store::ids::{CheckoutId, ContentKey, Digest128, FamilyId};
use aft_store::scan::{ContentKind, ContentSink, ScanPolicy, scan};
use aft_store::text::{
    ContentTrigrams, KNOWN, Scope, TextLocator, TextMembership, TextOverlay, TextSectionsBuilder,
    TextSegment, TextStatus, candidates, plan_pattern,
};
use rayon::prelude::*;

struct Collect(
    Mutex<TextSectionsBuilder>,
    Mutex<std::collections::HashSet<ContentKey>>,
);

impl ContentSink for Collect {
    fn read(&self, _path: &[u8], key: ContentKey, kind: ContentKind, bytes: &[u8]) {
        if !self.1.lock().unwrap().insert(key) {
            return;
        }
        let len = bytes.len() as u64;
        match kind {
            ContentKind::Text => {
                let trigrams = ContentTrigrams::of(bytes);
                self.0.lock().unwrap().add_indexed(key, len, &trigrams);
            }
            ContentKind::Binary => {
                self.0
                    .lock()
                    .unwrap()
                    .add_unindexed(key, len, TextStatus::Binary)
            }
            ContentKind::Large => {
                self.0
                    .lock()
                    .unwrap()
                    .add_unindexed(key, len, TextStatus::Oversize)
            }
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = Path::new(&args[1]);
    let store = Path::new(&args[2]);
    let corpus = Path::new(&args[3]);
    let max_patterns: usize = args.get(4).map_or(usize::MAX, |n| n.parse().unwrap());

    let policy = ScanPolicy::default();
    let sink = Collect(Mutex::default(), Mutex::default());
    let started = Instant::now();
    let cold = scan(root, &policy, None, &sink).unwrap();
    let cold_scan_ms = started.elapsed().as_secs_f64() * 1000.0;

    let family_id = FamilyId::from_digest(Digest128::of(b"text-gate"));
    let checkout = CheckoutId::from_digest(Digest128::of(root.as_os_str().as_encoded_bytes()));
    let family = Family::open(store, family_id).unwrap();
    let started = Instant::now();
    let builder = sink.0.into_inner().unwrap();
    let contents = builder.len();
    {
        let mut session = family.try_write().unwrap().unwrap();
        let mut pending = session.create_segment().unwrap();
        builder.write(pending.writer()).unwrap();
        let segment = session.finish_segment(pending).unwrap();
        let state = session.write_checkout_state(checkout, &cold.state).unwrap();
        session
            .publish(&Publication {
                add_segments: vec![segment],
                states: vec![state],
                ..Publication::default()
            })
            .unwrap();
    }
    let publish_ms = started.elapsed().as_secs_f64() * 1000.0;

    let started = Instant::now();
    let snapshot = family.snapshot().unwrap();
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let segments = snapshot
        .manifest()
        .segments()
        .iter()
        .map(|entry| {
            snapshot
                .open_segment(entry.id, &KNOWN, Arc::clone(&cache))
                .unwrap()
        })
        .collect::<Vec<_>>();
    let segment_bytes: u64 = snapshot.manifest().segments().iter().map(|e| e.len).sum();
    let texts = segments
        .iter()
        .map(|segment| TextSegment::open(segment).unwrap().unwrap())
        .collect::<Vec<_>>();
    let locator = TextLocator::build(&texts).unwrap();
    let state: CheckoutState = snapshot.checkout_state(checkout).unwrap().unwrap();
    let membership = TextMembership::build(&state, &locator);
    let bind_ms = started.elapsed().as_secs_f64() * 1000.0;
    let state_bytes = state.encode().len();

    // A second scan of the untouched checkout, as a restart would run it.
    let started = Instant::now();
    let warm = scan(
        root,
        &policy,
        Some(&state),
        &Collect(Mutex::default(), Mutex::default()),
    )
    .unwrap();
    let warm_scan_ms = started.elapsed().as_secs_f64() * 1000.0;

    // Every member's real bytes, for the brute-force answer.
    let files: Vec<(Vec<u8>, Vec<u8>)> = state
        .paths()
        .par_iter()
        .filter_map(|record| {
            let path = root.join(std::str::from_utf8(&record.path).ok()?);
            std::fs::read(path)
                .ok()
                .map(|bytes| (record.path.clone(), bytes))
        })
        .collect();

    let reader = BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(corpus).unwrap(),
    ));
    let mut patterns = BTreeSet::new();
    for line in reader.lines() {
        let line = line.unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        if let Some(pattern) = value["pattern"].as_str() {
            patterns.insert(pattern.to_string());
        }
    }
    let patterns: Vec<String> = patterns.into_iter().take(max_patterns).collect();

    let overlay = TextOverlay::new();
    let results: Vec<Option<(usize, usize, f64, bool)>> = patterns
        .par_iter()
        .map(|pattern| {
            let regex = regex::bytes::Regex::new(pattern).ok()?;
            let started = Instant::now();
            let query = plan_pattern(pattern, false).ok()?;
            let found =
                candidates(&state, &membership, &texts, &overlay, &query, Scope::All).unwrap();
            let micros = started.elapsed().as_secs_f64() * 1e6;
            let found: BTreeSet<&[u8]> = found.iter().map(Vec::as_slice).collect();
            let mut matches = 0;
            for (path, bytes) in &files {
                if regex.is_match(bytes) {
                    matches += 1;
                    if !found.contains(path.as_slice()) {
                        eprintln!(
                            "MISS {pattern:?}: {} matches and is not a candidate",
                            String::from_utf8_lossy(path)
                        );
                        return Some((found.len(), matches, micros, false));
                    }
                }
            }
            Some((found.len(), matches, micros, true))
        })
        .collect();

    let tested: Vec<_> = results.iter().flatten().collect();
    let misses = tested.iter().filter(|r| !r.3).count();
    let mut found: Vec<f64> = tested.iter().map(|r| r.0 as f64).collect();
    let mut matched: Vec<f64> = tested.iter().map(|r| r.1 as f64).collect();
    let mut micros: Vec<f64> = tested.iter().map(|r| r.2).collect();
    let all_files = tested.iter().filter(|r| r.0 >= state.paths().len()).count();
    found.sort_by(f64::total_cmp);
    matched.sort_by(f64::total_cmp);
    micros.sort_by(f64::total_cmp);
    let text_paths = state
        .paths()
        .iter()
        .filter(|record| record.kind == PathKind::Text)
        .count();

    let summary = serde_json::json!({
        "files": state.paths().len(),
        "text_paths": text_paths,
        "contents_indexed": contents,
        "cold_scan": { "ms": cold_scan_ms, "counters": format!("{:?}", cold.counters) },
        "warm_scan": { "ms": warm_scan_ms, "counters": format!("{:?}", warm.counters) },
        "publish_ms": publish_ms,
        "bind_ms": bind_ms,
        "segment_bytes": segment_bytes,
        "state_bytes": state_bytes,
        "patterns": patterns.len(),
        "tested": tested.len(),
        "unparsable": patterns.len() - tested.len(),
        "misses": misses,
        "every_file_candidate": all_files,
        "candidates": { "p50": percentile(&found, 0.5), "p90": percentile(&found, 0.9), "p99": percentile(&found, 0.99) },
        "matches": { "p50": percentile(&matched, 0.5), "p90": percentile(&matched, 0.9), "p99": percentile(&matched, 0.99) },
        "query_us": { "p50": percentile(&micros, 0.5), "p95": percentile(&micros, 0.95), "p99": percentile(&micros, 0.99), "max": micros.last().copied().unwrap_or(0.0) },
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    assert_eq!(misses, 0, "some matches were not candidates");
}
