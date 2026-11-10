//! Measures `trace_to` and `impact` answers on a real checkout: latency,
//! reply size, and paths returned or cut, over an even sample of function
//! and method symbols. Run it on two builds to compare them on the same
//! sample.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example trace_measure -- <checkout root> [sample] [threads]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout)]

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let sample = std::env::args()
        .nth(2)
        .map_or(240, |sample| sample.parse().unwrap());
    let threads = std::env::args()
        .nth(3)
        .map_or(8, |threads| threads.parse().unwrap());
    let report =
        aft::data_layer::trace_measure::run(std::path::Path::new(&root), threads, sample).unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
