//! Reproducible replace-all benchmark for the full side-effect-free edit preview path.
//!
//! Run from the workspace root with:
//! `cargo run --release -p agent-file-tools --example edit_match_perf_probe`

use aft::commands::edit_match::handle_edit_match;
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

#[path = "../tests/helpers/manifest_dir.rs"]
mod manifest_dir;

const OLD_TOKEN: &str = "TOKEN_OLD";
const NEW_TOKEN: &str = "TOKEN_NEW";

struct Args {
    bytes: usize,
    matches: usize,
    samples: usize,
    iterations: usize,
    output: Option<PathBuf>,
}

fn main() {
    let args = parse_args();
    assert!(args.matches > 0, "--matches must be positive");
    assert!(args.samples >= 3, "--samples must be at least 3");
    assert!(args.iterations > 0, "--iterations must be positive");

    let source = fixture_source(args.bytes, args.matches);
    let fixture = manifest_dir::manifest_dir()
        .join("target")
        .join("edit-match-perf-probe.ts");
    std::fs::create_dir_all(fixture.parent().expect("fixture parent")).expect("create fixture dir");
    std::fs::write(&fixture, &source).expect("write fixture");

    let request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "edit-match-perf-probe",
        "command": "edit_match",
        "file": fixture,
        "match": OLD_TOKEN,
        "replacement": NEW_TOKEN,
        "replace_all": true,
        "preview": true,
    }))
    .expect("construct edit_match request");
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

    let warm = handle_edit_match(&request, &ctx);
    assert_response(&warm, args.matches);
    let expected = serde_json::to_vec(&warm).expect("serialize warm response");
    if let Some(output) = &args.output {
        std::fs::write(output, &expected).expect("write response");
    }

    let mut per_call_ns = Vec::with_capacity(args.samples);
    for _ in 0..args.samples {
        let started = Instant::now();
        for _ in 0..args.iterations {
            let response = handle_edit_match(&request, &ctx);
            assert_response(&response, args.matches);
            black_box(response);
        }
        per_call_ns.push(started.elapsed().as_nanos() / args.iterations as u128);
    }
    per_call_ns.sort_unstable();

    let check = handle_edit_match(&request, &ctx);
    assert_response(&check, args.matches);
    let check_bytes = serde_json::to_vec(&check).expect("serialize check response");
    assert_eq!(
        check_bytes, expected,
        "edit_match output changed during measurement"
    );
    assert_eq!(
        std::fs::read(&fixture).expect("read fixture after probe"),
        source
    );

    println!("fixture_bytes={}", source.len());
    println!("matches={}", args.matches);
    println!("response_bytes={}", expected.len());
    println!("response_blake3={}", blake3::hash(&expected).to_hex());
    println!("samples_ns={per_call_ns:?}");
    println!("median_ns={}", per_call_ns[per_call_ns.len() / 2]);

    std::fs::remove_file(fixture).expect("remove fixture");
}

fn assert_response(response: &aft::protocol::Response, expected_matches: usize) {
    assert!(response.success, "edit_match failed: {:?}", response.data);
    assert_eq!(
        response
            .data
            .get("replacements")
            .and_then(serde_json::Value::as_u64),
        Some(expected_matches as u64),
    );
    assert_eq!(
        response
            .data
            .get("preview")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

fn fixture_source(target_bytes: usize, matches: usize) -> Vec<u8> {
    let minimum_chunk = OLD_TOKEN.len() + 2;
    assert!(
        target_bytes >= matches.saturating_mul(minimum_chunk),
        "--bytes must leave room for every match",
    );

    let chunk_bytes = target_bytes / matches;
    let mut source = Vec::with_capacity(target_bytes);
    for index in 0..matches {
        source.extend_from_slice(OLD_TOKEN.as_bytes());
        let target_end = if index + 1 == matches {
            target_bytes
        } else {
            (index + 1) * chunk_bytes
        };
        source.resize(target_end.saturating_sub(1), b'x');
        source.push(b'\n');
    }
    source
}

fn parse_args() -> Args {
    let mut args = Args {
        bytes: 1024 * 1024,
        matches: 1000,
        samples: 9,
        iterations: 5,
        output: None,
    };
    let mut raw = std::env::args().skip(1);

    while let Some(flag) = raw.next() {
        let value = raw
            .next()
            .unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--bytes" => args.bytes = value.parse().expect("parse --bytes"),
            "--matches" => args.matches = value.parse().expect("parse --matches"),
            "--samples" => args.samples = value.parse().expect("parse --samples"),
            "--iterations" => args.iterations = value.parse().expect("parse --iterations"),
            "--output" => args.output = Some(PathBuf::from(value)),
            _ => panic!("unknown argument: {flag}"),
        }
    }
    args
}
