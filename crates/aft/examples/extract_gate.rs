//! Extraction gate: the single-walk call-graph extractor against the one it
//! replaced, and incremental reparsing against full parsing.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example extract_gate -- repo <name> <root> <file list>
//! cargo run --release -p agent-file-tools --example extract_gate -- replay <edit pack.jsonl.gz>
//! ```
//!
//! `repo` compares both extractors on every file named in the list (one
//! path per line, relative to the root) and times each on 1 and 8 threads.
//! `replay` replays an edit pack, checking each incremental record against
//! a full extraction. Each prints one JSON object and exits non-zero when
//! any output differs.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::path::Path;

use aft::data_layer::extract_gate::{self, Extractor};

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("repo") => {
            let (name, root, list) = (&args[1], &args[2], &args[3]);
            let files = extract_gate::load(Path::new(root), Path::new(list)).unwrap();
            let comparison = extract_gate::compare(&files, 8);
            let mut timings = Vec::new();
            for threads in [1, 8] {
                for extractor in [Extractor::Reference, Extractor::SingleWalk] {
                    let timing = extract_gate::measure(&files, extractor, threads);
                    timings.push(serde_json::json!({
                        "extractor": format!("{extractor:?}"),
                        "timing": timing,
                    }));
                }
            }
            println!(
                "{}",
                serde_json::json!({
                    "repo": name,
                    "comparison": comparison,
                    "timings": timings,
                })
            );
            assert!(
                comparison.mismatches == 0 && comparison.error_mismatches == 0,
                "{name}: the extractors differ"
            );
        }
        Some("replay") => {
            let replay = extract_gate::replay(Path::new(&args[1])).unwrap();
            println!("{}", serde_json::to_string(&replay).unwrap());
            assert_eq!(
                replay.mismatches, 0,
                "an incremental record differs from a full extraction"
            );
        }
        _ => panic!("usage: extract_gate repo <name> <root> <list> | replay <pack>"),
    }
}
