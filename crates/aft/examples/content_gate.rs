//! Content-record gate on a real checkout.
//!
//! Scans a checkout, extracts every parsed file once per language with
//! AFT's extractor, stores each result as a content record, reads it back,
//! and checks the parse output it yields equals the extractor's own. Reports
//! sizes against today's JSON payloads, with and without compression, and
//! the time spent extracting, encoding and decoding.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example content_gate -- <checkout root>
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use aft::callgraph_store::join::CallgraphBlob;
use aft::data_layer::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use aft_store::content::{ContentRecord, RecordBody};
use aft_store::ids::ContentKey;
use aft_store::scan::{scan, ContentKind, ContentSink};
use flate2::write::ZlibEncoder;
use flate2::Compression;

#[derive(Default)]
struct Gate {
    seen: Mutex<HashSet<(ContentKey, String)>>,
    parsed: AtomicUsize,
    config: AtomicUsize,
    failed: AtomicUsize,
    mismatched: AtomicUsize,
    source_bytes: AtomicU64,
    json_bytes: AtomicU64,
    json_zlib_bytes: AtomicU64,
    surface_bytes: AtomicU64,
    body_bytes: AtomicU64,
    surface_zlib_bytes: AtomicU64,
    body_zlib_bytes: AtomicU64,
    extract_ns: AtomicU64,
    encode_ns: AtomicU64,
    decode_ns: Mutex<Vec<u64>>,
    json_decode_ns: Mutex<Vec<u64>>,
}

fn zlib(bytes: &[u8]) -> u64 {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap().len() as u64
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl ContentSink for Gate {
    fn read(&self, path: &[u8], key: ContentKey, kind: ContentKind, bytes: &[u8]) {
        if kind == ContentKind::Binary {
            return;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            return;
        };
        let Some(language) = language_for(path, bytes) else {
            return;
        };
        if !self.seen.lock().unwrap().insert((key, language.clone())) {
            return;
        }
        let started = Instant::now();
        let record = match extract(bytes, &language) {
            Ok(record) => record,
            Err(error) => {
                eprintln!("extract failed for {path}: {error}");
                self.failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        self.extract_ns
            .fetch_add(elapsed_ns(started), Ordering::Relaxed);
        if matches!(record.data, RecordBody::Config { .. }) {
            self.config.fetch_add(1, Ordering::Relaxed);
        } else {
            self.parsed.fetch_add(1, Ordering::Relaxed);
        }
        self.source_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);

        let blob = blob_from_record(&record).unwrap();
        let json = blob.to_bytes().unwrap();
        self.json_bytes
            .fetch_add(json.len() as u64, Ordering::Relaxed);
        self.json_zlib_bytes
            .fetch_add(zlib(&json), Ordering::Relaxed);

        let started = Instant::now();
        let (surface, body) = record.encode();
        self.encode_ns
            .fetch_add(elapsed_ns(started), Ordering::Relaxed);
        self.surface_bytes
            .fetch_add(surface.len() as u64, Ordering::Relaxed);
        self.body_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        self.surface_zlib_bytes
            .fetch_add(zlib(&surface), Ordering::Relaxed);
        self.body_zlib_bytes
            .fetch_add(zlib(&body), Ordering::Relaxed);

        let started = Instant::now();
        let decoded = ContentRecord::decode(&surface, &body).unwrap();
        let back = blob_from_record(&decoded).unwrap();
        self.decode_ns.lock().unwrap().push(elapsed_ns(started));
        let started = Instant::now();
        let from_json = CallgraphBlob::from_bytes(&json).unwrap();
        self.json_decode_ns
            .lock()
            .unwrap()
            .push(elapsed_ns(started));

        if back != blob || from_json != blob || decoded != record {
            eprintln!("MISMATCH {path} ({language})");
            self.mismatched.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn percentile(values: &mut [u64], p: f64) -> u64 {
    values.sort_unstable();
    if values.is_empty() {
        return 0;
    }
    values[((values.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let gate = Gate::default();
    let started = Instant::now();
    let report = scan(
        Path::new(&root),
        &aft::data_layer::content::scan_policy(),
        None,
        &gate,
    )
    .unwrap();
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut decode = gate.decode_ns.into_inner().unwrap();
    let mut json_decode = gate.json_decode_ns.into_inner().unwrap();
    let mb = |value: &AtomicU64| value.load(Ordering::Relaxed) as f64 / 1e6;
    let summary = serde_json::json!({
        "extractor": EXTRACTOR_VERSION,
        "files": report.counters.files,
        "parsed_records": gate.parsed.load(Ordering::Relaxed),
        "config_records": gate.config.load(Ordering::Relaxed),
        "failed": gate.failed.load(Ordering::Relaxed),
        "mismatched": gate.mismatched.load(Ordering::Relaxed),
        "wall_ms": wall_ms,
        "extract_cpu_ms": gate.extract_ns.load(Ordering::Relaxed) as f64 / 1e6,
        "encode_cpu_ms": gate.encode_ns.load(Ordering::Relaxed) as f64 / 1e6,
        "mb": {
            "source": mb(&gate.source_bytes),
            "json_today": mb(&gate.json_bytes),
            "json_today_zlib": mb(&gate.json_zlib_bytes),
            "surface": mb(&gate.surface_bytes),
            "body": mb(&gate.body_bytes),
            "surface_zlib": mb(&gate.surface_zlib_bytes),
            "body_zlib": mb(&gate.body_zlib_bytes),
        },
        "decode_us": {
            "record_p50": percentile(&mut decode, 0.5) as f64 / 1e3,
            "record_p99": percentile(&mut decode, 0.99) as f64 / 1e3,
            "record_total_ms": decode.iter().sum::<u64>() as f64 / 1e6,
            "json_p50": percentile(&mut json_decode, 0.5) as f64 / 1e3,
            "json_p99": percentile(&mut json_decode, 0.99) as f64 / 1e3,
            "json_total_ms": json_decode.iter().sum::<u64>() as f64 / 1e6,
        },
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    assert_eq!(
        gate.failed.load(Ordering::Relaxed),
        0,
        "an extraction failed"
    );
    assert!(
        gate.parsed.load(Ordering::Relaxed) > 0,
        "no file was extracted"
    );
    assert_eq!(gate.mismatched.load(Ordering::Relaxed), 0);
}
