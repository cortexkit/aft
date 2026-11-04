//! Scans a checkout and reports the size of its encoded checkout state.
//!
//! ```text
//! cargo run --release -p aft-store --example state_size -- <checkout root>
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "a measurement tool: it stops on any failure and prints its result"
)]

use aft_store::ids::ContentKey;
use aft_store::scan::{ContentKind, ContentSink, ScanPolicy, scan};

struct Ignore;

impl ContentSink for Ignore {
    fn read(&self, _: &[u8], _: ContentKey, _: ContentKind, _: &[u8]) {}
}

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let report = scan(
        std::path::Path::new(&root),
        &ScanPolicy::default(),
        None,
        &Ignore,
    )
    .unwrap();
    let paths = report.state.paths().len();
    let bytes = report.state.encode().len();
    println!(
        "{paths} paths, {bytes} bytes encoded, {:.1} bytes per path",
        bytes as f64 / paths as f64
    );
}
