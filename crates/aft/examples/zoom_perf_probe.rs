use aft::commands::zoom::handle_zoom;
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

#[path = "../tests/helpers/manifest_dir.rs"]
mod manifest_dir;

struct Args {
    file: PathBuf,
    symbol: String,
    samples: usize,
    iterations: usize,
    output: Option<PathBuf>,
}

fn main() {
    let args = parse_args();
    assert!(args.samples >= 3, "--samples must be at least 3");
    assert!(args.iterations > 0, "--iterations must be positive");

    let fixture = std::fs::read(&args.file).expect("read zoom fixture");
    let fixture_lines = fixture.split(|byte| *byte == b'\n').count();
    let file = args.file.to_string_lossy();
    let request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "zoom-perf-probe",
        "command": "zoom",
        "file": file,
        "symbol": args.symbol,
        "context_lines": 3,
    }))
    .expect("construct zoom request");
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

    let warm = handle_zoom(&request, &ctx);
    assert!(warm.success, "warm zoom failed: {:?}", warm.data);
    let expected = serde_json::to_vec(&warm).expect("serialize warm response");
    if let Some(output) = &args.output {
        std::fs::write(output, &expected).expect("write zoom response");
    }

    let mut per_call_ns = Vec::with_capacity(args.samples);
    for _ in 0..args.samples {
        let started = Instant::now();
        for _ in 0..args.iterations {
            let response = handle_zoom(&request, &ctx);
            assert!(response.success, "timed zoom failed: {:?}", response.data);
            black_box(response);
        }
        per_call_ns.push(started.elapsed().as_nanos() / args.iterations as u128);
    }
    per_call_ns.sort_unstable();

    let check = serde_json::to_vec(&handle_zoom(&request, &ctx)).expect("serialize check response");
    assert_eq!(check, expected, "zoom output changed during measurement");

    println!("fixture_bytes={}", fixture.len());
    println!("fixture_lines={fixture_lines}");
    println!("response_bytes={}", expected.len());
    println!("response_blake3={}", blake3::hash(&expected).to_hex());
    println!("samples_ns={per_call_ns:?}");
    println!("median_ns={}", per_call_ns[per_call_ns.len() / 2]);
}

fn parse_args() -> Args {
    let mut file = manifest_dir::manifest_dir().join("src/parser.rs");
    let mut symbol = "extract_symbols_from_tree".to_string();
    let mut samples = 9usize;
    let mut iterations = 100usize;
    let mut output = None;
    let mut raw = std::env::args().skip(1);

    while let Some(flag) = raw.next() {
        let value = raw
            .next()
            .unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--file" => file = PathBuf::from(value),
            "--symbol" => symbol = value,
            "--samples" => samples = value.parse().expect("parse --samples"),
            "--iterations" => iterations = value.parse().expect("parse --iterations"),
            "--output" => output = Some(PathBuf::from(value)),
            _ => panic!("unknown argument: {flag}"),
        }
    }

    Args {
        file,
        symbol,
        samples,
        iterations,
        output,
    }
}
